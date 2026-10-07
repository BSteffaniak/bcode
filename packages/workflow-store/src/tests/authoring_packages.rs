//! Existing authoring packages behavioral acceptance.
use super::*;
use crate::database::block_on;
use std::time::Duration;
use switchy_database::query::{SelectQuery, add, count_all, identifier, zero_blob};
use switchy_database::schema::create_trigger;

#[test]
fn publication_read_fence_rejects_local_external_and_reopened_state() {
    let root = tempfile::tempdir().unwrap();
    let store = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    let fence = store.publication_read_fence().unwrap();
    store.validate_publication_read_fence(&fence).unwrap();
    // The timestamp is intentionally irrelevant: invalidation follows commits, not clocks.
    block_on(
        insert("workflow_package_publications")
            .value("package_id", "local")
            .value("package_lock_digest_sha256", "digest")
            .value("lock_json", "{}")
            .value("receipt_json", "{}")
            .value("published_at_ms", 100)
            .execute(&store.connection),
    )
    .unwrap();
    assert!(store.validate_publication_read_fence(&fence).is_err());
    let fence = store.publication_read_fence().unwrap();
    let external = database::open(
        &store.path,
        switchy_database_connection::SqliteAccess::ReadWrite,
        Duration::from_secs(5),
        true,
    )
    .unwrap();
    block_on(
        insert("workflow_package_publications")
            .value("package_id", "external")
            .value("package_lock_digest_sha256", "digest")
            .value("lock_json", "{}")
            .value("receipt_json", "{}")
            .value("published_at_ms", 1)
            .execute(&external),
    )
    .unwrap();
    assert!(store.validate_publication_read_fence(&fence).is_err());
    let fence = store.publication_read_fence().unwrap();
    store.validate_publication_read_fence(&fence).unwrap();
    drop(external);
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    assert!(reopened.validate_publication_read_fence(&fence).is_err());
}
#[test]
#[allow(clippy::too_many_lines)]
fn package_apply_is_atomic_and_generation_guarded() {
    let (workflow, draft, catalog) = authored_store_fixture();
    let mut second_document = draft.document.clone();
    second_document.workflow_id = "authored/second".to_string();
    second_document.metadata.title = "Second".to_string();
    second_document.definition.name = "second".to_string();
    let member = |member_id: &str, document: WorkflowAuthoringDocument| {
        let identity = WorkflowDefinitionIdentity::for_definition(
            document.workflow_id.clone(),
            &document.definition,
        )
        .expect("identity");
        let source_map = bcode_workflow::WorkflowSourceMap {
            version: bcode_workflow::WORKFLOW_SOURCE_MAP_VERSION,
            entries: Vec::new(),
        };
        bcode_workflow::WorkflowPackagePlannedMember {
            member_id: member_id.to_string(),
            member_source_map: bcode_workflow::WorkflowPackageMemberSourceMap {
                version: bcode_workflow::WORKFLOW_PACKAGE_MEMBER_SOURCE_MAP_VERSION,
                member_id: member_id.to_string(),
                source_name: format!("{member_id}.json"),
                source_map: source_map.clone(),
            },
            lowering: bcode_workflow::WorkflowSourceLoweringResult {
                version: bcode_workflow::WORKFLOW_SOURCE_LOWERING_VERSION,
                profile: bcode_workflow::WorkflowSourceProfile::Canonical,
                validation: document.validation_report(),
                document,
                source_map,
            },
            definition_identity: identity,
            dependency_closure: Vec::new(),
        }
    };
    let members = vec![
        member("first", draft.document),
        member("second", second_document),
    ];
    let mut locked = members
        .iter()
        .map(|member| bcode_workflow::WorkflowPackageLockedMember {
            member_id: member.member_id.clone(),
            source_digest_sha256: "a".repeat(64),
            executable_digest_sha256: "b".repeat(64),
            definition_identity: member.definition_identity.clone(),
            published_revision: None,
            dependency_closure: Vec::new(),
        })
        .collect::<Vec<_>>();
    locked.sort_by(|left, right| left.member_id.cmp(&right.member_id));
    let request = WorkflowPackageApplyRequest {
        version: bcode_workflow::WORKFLOW_PACKAGE_MUTATION_VERSION,
        plan: bcode_workflow::WorkflowPackagePlan {
            version: bcode_workflow::WORKFLOW_PACKAGE_PLAN_VERSION,
            package_id: "example/package".to_string(),
            members,
            lock: bcode_workflow::WorkflowPackageLock {
                version: bcode_workflow::WORKFLOW_PACKAGE_LOCK_VERSION,
                package_id: "example/package".to_string(),
                package_source_digest_sha256: "c".repeat(64),
                imports: Vec::new(),
                exports: locked
                    .first()
                    .map(|member| bcode_workflow::WorkflowPackageLockedExport {
                        export: "main".to_string(),
                        member_id: member.member_id.clone(),
                        definition_identity: member.definition_identity.clone(),
                        published_revision: None,
                    })
                    .into_iter()
                    .collect(),
                members: locked,
            },
        },
        expected_generations: Vec::new(),
    };
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    assert!(
        store
            .apply_workflow_package_with_fault(
                &request,
                9,
                &RejectPackageMutationBoundary(WorkflowPackageMutationBoundary::AllMembersStaged,),
            )
            .is_err()
    );
    for member in &request.plan.members {
        assert!(
            store
                .workflow_draft(&member.lowering.document.workflow_id, "package")
                .expect("rolled back draft")
                .is_none()
        );
    }
    let applied = store.apply_workflow_package(&request, 10).expect("apply");
    assert_eq!(applied.members.len(), 2);
    assert_eq!(
        store
            .workflow_draft(&workflow.workflow_id, "package")
            .expect("first draft")
            .expect("first")
            .generation,
        1
    );
    let mut conflicting = request.clone();
    conflicting.expected_generations = vec![
        bcode_workflow::WorkflowPackageExpectedGeneration {
            member_id: "first".to_string(),
            expected_generation: 1,
        },
        bcode_workflow::WorkflowPackageExpectedGeneration {
            member_id: "second".to_string(),
            expected_generation: 99,
        },
    ];
    assert!(store.apply_workflow_package(&conflicting, 11).is_err());
    assert_eq!(
        store
            .workflow_draft(&workflow.workflow_id, "package")
            .expect("first draft")
            .expect("first")
            .generation,
        1
    );
    assert!(catalog.version > 0);
}
#[test]
#[allow(clippy::too_many_lines)]
fn package_publish_is_atomic_and_generates_authoritative_lock() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let (workflow, mut draft, _) = authored_store_fixture();
    draft.draft_id = "package".to_string();
    let identity = WorkflowDefinitionIdentity::for_definition(
        draft.document.workflow_id.clone(),
        &draft.document.definition,
    )
    .expect("identity");
    let source_digest = draft
        .document
        .source_digest_sha256()
        .expect("source digest");
    let executable_digest = draft
        .document
        .executable_source_digest_sha256()
        .expect("executable digest");
    store.create_authored_workflow(&workflow).expect("workflow");
    store.create_workflow_draft(&draft).expect("draft");
    let locked_member = bcode_workflow::WorkflowPackageLockedMember {
        member_id: "main".to_string(),
        source_digest_sha256: source_digest,
        executable_digest_sha256: executable_digest,
        definition_identity: identity.clone(),
        published_revision: None,
        dependency_closure: Vec::new(),
    };
    let lock = WorkflowPackageLock {
        version: bcode_workflow::WORKFLOW_PACKAGE_LOCK_VERSION,
        package_id: "example/package".to_string(),
        package_source_digest_sha256: "a".repeat(64),
        imports: Vec::new(),
        exports: vec![bcode_workflow::WorkflowPackageLockedExport {
            export: "main".to_string(),
            member_id: locked_member.member_id.clone(),
            definition_identity: locked_member.definition_identity.clone(),
            published_revision: None,
        }],
        members: vec![locked_member],
    };
    let request = WorkflowPackagePublishRequest {
        version: bcode_workflow::WORKFLOW_PACKAGE_MUTATION_VERSION,
        package_id: lock.package_id.clone(),
        expected_lock: lock,
        expected_generations: vec![bcode_workflow::WorkflowPackageExpectedGeneration {
            member_id: "main".to_string(),
            expected_generation: 1,
        }],
    };
    assert!(
        store
            .publish_workflow_package_with_fault(
                &request,
                19,
                &RejectPackageMutationBoundary(WorkflowPackageMutationBoundary::AllMembersStaged,),
            )
            .is_err()
    );
    assert!(
        store
            .workflow_revision(&workflow.workflow_id, 1)
            .expect("rolled back revision")
            .is_none()
    );
    assert!(
        store
            .workflow_package_publication("example/package", None)
            .expect("rolled back receipt")
            .is_none()
    );
    assert_eq!(
        store
            .workflow_draft(&workflow.workflow_id, "package")
            .expect("draft retained")
            .expect("draft")
            .generation,
        1
    );
    let published = store
        .publish_workflow_package(&request, 20)
        .expect("publish package");
    assert_eq!(published.outcome, WorkflowPackageMutationOutcome::Published);
    assert_eq!(published.members.len(), 1);
    assert_eq!(
        published.members[0]
            .revision
            .as_ref()
            .expect("revision")
            .revision,
        1
    );
    assert!(
        published.lock.as_ref().expect("lock").members[0]
            .published_revision
            .is_some()
    );
    assert!(
        store
            .workflow_draft(&workflow.workflow_id, "package")
            .expect("draft read")
            .is_none()
    );

    let published_lock = published.lock.expect("published lock");
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert_eq!(
        reopened
            .workflow_revision(&workflow.workflow_id, 1)
            .expect("revision after restart")
            .expect("revision")
            .definition_identity,
        identity
    );
    assert_eq!(
        published_lock.members[0]
            .published_revision
            .as_ref()
            .expect("lock revision")
            .revision,
        1
    );
    assert_eq!(
        published_lock.exports[0]
            .published_revision
            .as_ref()
            .expect("export revision")
            .revision,
        1
    );
    let receipt = reopened
        .workflow_package_publication("example/package", None)
        .expect("receipt after restart")
        .expect("receipt");
    assert_eq!(receipt.exports, published_lock.exports);
    assert_eq!(
        reopened
            .workflow_package_lock("example/package", &receipt.package_lock_digest_sha256)
            .unwrap(),
        Some(published_lock)
    );
    assert!(
        reopened
            .workflow_package_lock("example/package", &"0".repeat(64))
            .unwrap()
            .is_none()
    );
    assert_eq!(receipt.published_at_ms, 20);
    let mut store = reopened;
    let mut run = new_run();
    run.definition_id
        .clone_from(&receipt.exports[0].definition_identity.definition_id);
    run.input = Some(serde_json::json!({"message": "test"}));
    run.definition_version = receipt.exports[0].definition_identity.definition_version;
    let binding = Some((
        receipt.package_id.as_str(),
        receipt.package_lock_digest_sha256.as_str(),
    ));
    assert!(store.create_run_with_package(&run, binding).unwrap());
    assert!(!store.create_run_with_package(&run, binding).unwrap());
    assert!(store.create_run_with_package(&run, None).is_err());
    let retained_rows = database::rows(
        &store.connection,
        &select("workflow_run_packages")
            .columns(&["package_id", "lock_digest"])
            .where_eq("run_id", run.run_id.as_str()),
    )
    .unwrap();
    let retained_row = retained_rows.first().unwrap();
    let retained = (
        database::text(retained_row, "package_id").unwrap(),
        database::text(retained_row, "lock_digest").unwrap(),
    );
    assert_eq!(
        retained,
        (
            receipt.package_id.clone(),
            receipt.package_lock_digest_sha256.clone()
        )
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("bound run restart");
    assert_eq!(
        store
            .resolve_run_package_member(&run.run_id, &receipt.exports[0].member_id)
            .unwrap(),
        receipt.exports[0].definition_identity,
    );
    assert!(!store.create_run_with_package(&run, binding).unwrap());
    assert!(store.create_run_with_package(&run, None).is_err());
    verify_package_replacement(&mut store, &run, &receipt);
    assert!(
        store
            .resolve_run_package_member(&run.run_id, "missing-member")
            .is_err()
    );
    let mut stale = request;
    stale.expected_generations[0].expected_generation = 2;
    assert!(store.publish_workflow_package(&stale, 21).is_err());
    assert_eq!(
        store
            .workflow_revision(&workflow.workflow_id, 1)
            .expect("revision read")
            .expect("revision")
            .definition_identity,
        identity
    );
}
#[test]
fn authorization_profile_is_atomic_idempotent_and_restart_safe() {
    let temp = tempfile::tempdir().expect("temp");
    let run = new_run();
    {
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        assert!(store.create_run_idempotent(&run).expect("create"));
        assert!(!store.create_run_idempotent(&run).expect("repeat"));
    }
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .authorization_profile,
        run.authorization_profile
    );
    let conflicting = NewWorkflowRun {
        authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
            policy_digest_sha256: "b".repeat(64),
            ..run.authorization_profile.clone()
        },
        ..run
    };
    assert!(store.create_run_idempotent(&conflicting).is_err());
}
#[test]
fn authored_run_provenance_is_atomic_idempotent_and_restart_safe() {
    let temp = tempfile::tempdir().expect("temp");
    let run = NewWorkflowRun {
        authored_provenance: Some(authored_run_provenance()),
        ..new_run()
    };
    {
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        persist_authored_run_provenance_fixture(&store);
        assert!(store.create_run_idempotent(&run).expect("create"));
        assert!(!store.create_run_idempotent(&run).expect("repeat"));
    }
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .authored_provenance,
        run.authored_provenance
    );
    let conflicting = NewWorkflowRun {
        authored_provenance: Some(AuthoredWorkflowRunProvenance {
            configuration: serde_json::json!({"message": "different"}),
            ..authored_run_provenance()
        }),
        ..run
    };
    assert!(store.create_run_idempotent(&conflicting).is_err());
}
#[test]
fn authored_run_provenance_rejects_secret_bearing_configuration_before_persistence() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    persist_authored_run_provenance_fixture(&store);
    for configuration in [
        serde_json::json!({"api_key": "must-not-persist"}),
        serde_json::json!({"token": {"backend": "env", "name": "TOKEN"}}),
    ] {
        let run = NewWorkflowRun {
            authored_provenance: Some(AuthoredWorkflowRunProvenance {
                configuration,
                ..authored_run_provenance()
            }),
            ..new_run()
        };
        assert!(store.create_run(&run).is_err());
        assert!(store.run_summary("run-1").expect("summary").is_none());
    }
}
#[test]
fn authored_run_provenance_definition_mismatch_fails_before_persistence() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    persist_authored_run_provenance_fixture(&store);
    let run = NewWorkflowRun {
        authored_provenance: Some(AuthoredWorkflowRunProvenance {
            definition_identity: bcode_workflow::WorkflowDefinitionIdentity {
                kind: "authored/example".to_string(),
                definition_id: "different".to_string(),
                definition_version: 1,
            },
            ..authored_run_provenance()
        }),
        ..new_run()
    };
    assert!(store.create_run(&run).is_err());
    assert!(store.run_summary("run-1").expect("summary").is_none());
}
#[test]
fn package_replacement_crash_child() {
    let Ok(path) = std::env::var("BCODE_TEST_PACKAGE_REPLACEMENT_DB") else {
        return;
    };
    let mut store = WorkflowStore::open_at_path(Path::new(&path)).expect("store");
    let run_id = std::env::var("BCODE_TEST_PACKAGE_REPLACEMENT_RUN").expect("run");
    let owner = store
        .execution_authority(&run_id)
        .expect("authority")
        .expect("owner");
    let successor = store
        .pending_replacement(&run_id)
        .expect("pending")
        .expect("intent");
    store
        .complete_leaf_replacement_owned(&run_id, &owner, &successor, 31)
        .expect("handoff");
    std::process::exit(74);
}
#[test]
fn replacement_rejects_existing_successor_and_foreign_authority_before_cancellation() {
    let (_temp, mut store, existing, authority, _) = connected_publication_fixture();
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let old = NewWorkflowRun {
        run_id: "old".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    let mut successor = NewWorkflowRun {
        run_id: existing.run_id,
        ..old
    };
    assert!(
        store
            .request_replacement_owned("old", &authority, &successor, 20)
            .is_err()
    );
    successor.run_id = "successor".into();
    for owner in [
        None,
        Some(WorkflowExecutionAuthority {
            daemon_instance_id: "foreign".into(),
            ..authority.clone()
        }),
    ] {
        successor.execution_authority = owner;
        assert!(
            store
                .request_replacement_owned("old", &authority, &successor, 21)
                .is_err()
        );
    }
    assert!(
        store
            .pending_replacement("old")
            .expect("no intent")
            .is_none()
    );
    assert_eq!(
        store
            .run_summary("old")
            .expect("summary")
            .expect("old")
            .cancellation_requested_at_ms,
        None
    );
    successor.execution_authority = Some(authority.clone());
    assert!(
        store
            .request_replacement_owned("old", &authority, &successor, 22)
            .expect("valid")
    );
}
#[test]
fn inherited_cancellation_requires_child_authority_and_parent_intent() {
    let (_temp, mut store) = initialized_store();
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "child-owner".into(),
        generation: 1,
        fencing_token: "child-fence".into(),
    };
    let child = NewWorkflowRun {
        run_id: "child".into(),
        execution_authority: Some(authority.clone()),
        ..new_run()
    };
    store.create_run(&child).expect("child");
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "example",
        &definition("example"),
    )
    .expect("identity");
    let target =
        serde_json::to_string(&bcode_workflow::WorkflowCallTarget::Definition { identity })
            .expect("target");
    block_on(
        insert("workflow_run_links")
            .value("root_run_id", "run-1")
            .value("parent_run_id", "run-1")
            .value("parent_node_id", "review")
            .value("parent_activation_id", "activation")
            .value("parent_attempt", 1)
            .value("child_run_id", "child")
            .value("version", WORKFLOW_RUN_LINK_VERSION)
            .value("target_json", target.as_str())
            .value("depth", 2)
            .value("created_at_ms", 0)
            .execute(&store.connection),
    )
    .expect("link");
    assert!(
        !store
            .inherit_parent_cancellation_owned("child", &authority, 20)
            .expect("no parent intent")
    );
    store
        .request_cancellation("run-1", 21)
        .expect("parent cancellation");
    let stale = WorkflowExecutionAuthority {
        fencing_token: "parent-fence".into(),
        ..authority.clone()
    };
    assert!(
        store
            .inherit_parent_cancellation_owned("child", &stale, 22)
            .is_err()
    );
    assert!(
        store
            .run_summary("child")
            .expect("summary")
            .expect("child")
            .cancellation_requested_at_ms
            .is_none()
    );
    assert!(
        store
            .inherit_parent_cancellation_owned("child", &authority, 23)
            .expect("inherited")
    );
    assert_eq!(
        store
            .run_summary("child")
            .expect("summary")
            .expect("child")
            .status,
        RunStatus::Cancelled
    );
    assert!(
        !store
            .inherit_parent_cancellation_owned("child", &authority, 24)
            .expect("idempotent")
    );
}
#[test]
fn publication_cancels_running_fan_out_with_unstarted_members() {
    exercise_fan_out_retirement(None);
}
#[test]
fn publication_preserves_competing_fan_out_success() {
    exercise_fan_out_retirement(Some(false));
}
#[test]
fn publication_preserves_competing_fan_out_failure() {
    exercise_fan_out_retirement(Some(true));
}
#[test]
fn quiescent_publication_retires_removed_edges_across_restart() {
    verify_edge_removal_publication(false);
}
#[test]
fn active_publication_retires_removed_edges_across_restart() {
    verify_edge_removal_publication(true);
}
#[test]
fn leaf_publication_cancels_unstarted_activation_and_survives_reopen() {
    let (temp, mut store) = initialized_store();
    seed_unstarted_leaf_owner(&store);
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let (node_id, activation_id): (String, String) =
        fixture_activation(&store).expect("activation");
    let mut node = store
        .current_run_graph_node("run-1", &node_id)
        .expect("node")
        .expect("node")
        .node;
    node.id = "replacement-leaf".into();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "run-1".into(),
        mutation_id: "cancel-leaf".into(),
        expected_revision: 1,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::AddNode {
            node,
            entry: true,
            exit: true,
        }],
        reconciliation: vec![bcode_workflow::WorkflowRunGraphReconciliation::Cancel {
            activation_id: activation_id.clone(),
        }],
    };
    store
        .stage_run_graph_edit(&request, &authority, 20)
        .expect("stage");
    block_on(
        update("workflow_activations")
            .value("status", "running")
            .where_eq("activation_id", activation_id.as_str())
            .execute(&store.connection),
    )
    .expect("dispatched fixture");
    assert!(
        store
            .publish_retained_leaf_run_graph_edit("run-1", "cancel-leaf", &authority, 21)
            .is_err()
    );
    block_on(
        update("workflow_activations")
            .value("status", "pending")
            .where_eq("activation_id", activation_id.as_str())
            .execute(&store.connection),
    )
    .expect("unstarted fixture");
    install_cancel_publication_fault(&store);
    assert!(
        store
            .publish_retained_leaf_run_graph_edit("run-1", "cancel-leaf", &authority, 21)
            .is_err()
    );
    let status: String = authoring_fixture_text(
        &store,
        select("workflow_activations")
            .columns(&["status"])
            .where_eq("activation_id", activation_id.as_str()),
        "status",
    )
    .expect("rolled back activation");
    assert_eq!(status, "pending");
    assert_eq!(
        run_graph::graph_revision(&store.connection, "run-1").expect("revision"),
        Some(1)
    );
    let replacement = store
        .current_run_graph_node("run-1", "replacement-leaf")
        .expect("rolled back node");
    assert!(replacement.is_none());
    block_on(
        store
            .connection
            .drop_trigger("reject_cancel_publication", false),
    )
    .expect("remove fault");
    assert_eq!(
        store
            .publish_retained_leaf_run_graph_edit("run-1", "cancel-leaf", &authority, 21)
            .expect("publish"),
        2
    );
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let status: String = authoring_fixture_text(
        &store,
        select("workflow_activations")
            .columns(&["status"])
            .where_eq("activation_id", activation_id.as_str()),
        "status",
    )
    .expect("status");
    assert_eq!(status, "cancelled");
    assert_published_leaf_admission(&store);
}
#[test]
fn retained_leaf_publication_preserves_admission_and_settles() {
    let (temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("owner");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let (node_id, activation_id): (String, String) =
        fixture_activation(&store).expect("activation");
    let mut node = store
        .current_run_graph_node("run-1", &node_id)
        .expect("node")
        .expect("node")
        .node;
    node.id = "new-leaf".to_string();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "run-1".to_string(),
        mutation_id: "retain-leaf".to_string(),
        expected_revision: 1,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::AddNode {
            node,
            entry: true,
            exit: true,
        }],
        reconciliation: vec![bcode_workflow::WorkflowRunGraphReconciliation::Retain {
            activation_id: activation_id.clone(),
        }],
    };
    store
        .stage_run_graph_edit(&request, &authority, 20)
        .expect("stage");
    assert_eq!(
        store
            .publish_retained_leaf_run_graph_edit("run-1", "retain-leaf", &authority, 21)
            .expect("publish"),
        2
    );
    assert_eq!(
        store
            .activation_admitted_graph_revision("run-1", &node_id, &activation_id)
            .expect("admission"),
        Some(1)
    );
    assert!(
        run_graph::revised_leaf_exit(&store.connection, "run-1", &node_id, &activation_id)
            .expect("settlement")
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(
        run_graph::revised_leaf_exit(&store.connection, "run-1", &node_id, &activation_id)
            .expect("retained settlement")
    );
    assert_eq!(
        store
            .publish_retained_leaf_run_graph_edit("run-1", "retain-leaf", &authority, 22)
            .expect("duplicate"),
        2
    );
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "retained-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: node_id.clone(),
            activation_id: activation_id.clone(),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(1),
            artifact_reference: None,
            created_at_ms: 23,
        })
        .expect("commit retained output");
    let status: String = authoring_fixture_text(
        &store,
        select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1")
            .where_eq("activation_id", activation_id.as_str()),
        "status",
    )
    .expect("retained status");
    assert_eq!(status, "completed");
    assert_eq!(
        store
            .activation_admitted_graph_revision("run-1", &node_id, &activation_id)
            .expect("historical admission"),
        Some(1)
    );
}
#[test]
fn execution_authority_transfer_is_generation_fenced() {
    let (_temp, mut store) = initialized_store();
    let initial = WorkflowExecutionAuthority {
        target_artifact_id: "artifact-a".to_string(),
        daemon_instance_id: "daemon-a".to_string(),
        generation: 1,
        fencing_token: "token-a".to_string(),
    };
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", initial.target_artifact_id.as_str())
            .value(
                "coordinator_daemon_instance_id",
                initial.daemon_instance_id.as_str(),
            )
            .value("coordinator_generation", initial.generation)
            .value("coordinator_fencing_token", initial.fencing_token.as_str())
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("seed authority");
    let replacement = WorkflowExecutionAuthority {
        target_artifact_id: "artifact-a".to_string(),
        daemon_instance_id: "daemon-b".to_string(),
        generation: 2,
        fencing_token: "token-b".to_string(),
    };

    store
        .transfer_execution_authority("run-1", &initial, &replacement, 20)
        .expect("transfer");
    assert_eq!(
        store.execution_authority("run-1").expect("authority"),
        Some(replacement.clone())
    );
    assert!(
        store
            .transfer_execution_authority("run-1", &initial, &replacement, 21)
            .is_err()
    );
    assert!(store.verify_execution_authority("run-1", &initial).is_err());
    store
        .verify_execution_authority("run-1", &replacement)
        .expect("current authority");
}
#[test]
#[allow(clippy::too_many_lines)]
fn child_creation_rejects_recursive_target_and_authority_escalation() {
    let (_temp, mut store) = initialized_store();
    let parent_definition = definition("example");
    let parent_identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        parent_definition.name.clone(),
        &parent_definition,
    )
    .expect("identity");
    let call_definition = workflow_call_definition(parent_identity.clone());
    store
        .persist_definition("call-parent", 1, &call_definition)
        .expect("parent definition");
    store
        .persist_definition(
            &parent_identity.definition_id,
            parent_identity.definition_version,
            &parent_definition,
        )
        .expect("target definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "recursive-parent".to_string(),
            definition_id: "call-parent".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::ReadOnly,
            limits: WorkflowRunLimits::default(),
        })
        .expect("parent run");
    let activation = store
        .pending_activations(10)
        .expect("pending")
        .pop()
        .expect("call");
    store
        .prepare_pending_activation(
            "recursive-parent",
            "call",
            &activation.activation_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({}),
            2,
        )
        .expect("prepare");
    let target = bcode_workflow::WorkflowCallTarget::Definition {
        identity: parent_identity.clone(),
    };
    let child_run_id = workflow_child_run_id(
        "recursive-parent",
        "recursive-parent",
        &activation.activation_id,
        1,
        &target,
    );
    let mut request = NewChildWorkflowRun {
        link: WorkflowRunLink {
            version: WORKFLOW_RUN_LINK_VERSION,
            root_run_id: "recursive-parent".to_string(),
            parent_run_id: "recursive-parent".to_string(),
            parent_node_id: "call".to_string(),
            parent_activation_id: activation.activation_id,
            parent_attempt: 1,
            child_run_id: child_run_id.clone(),
            target,
            depth: 2,
            created_at_ms: 3,
        },
        run: NewWorkflowRun {
            run_id: child_run_id.clone(),
            definition_id: parent_identity.definition_id,
            definition_version: parent_identity.definition_version,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 3,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        },
    };
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert!(store.run_summary(&child_run_id).expect("child").is_none());
    request.run.authorization_ceiling = bcode_workflow::WorkflowToolCapability::ReadOnly;
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert!(store.run_summary(&child_run_id).expect("child").is_none());
}
#[test]
fn pending_publication_acceptance_is_atomic_and_idempotent() {
    exercise_pending_publication(true);
}
#[test]
fn pending_publication_finalizes_only_after_exact_settlement() {
    exercise_pending_publication(false);
}
#[test]
fn explicit_cancellation_repair_unblocks_accepted_publication() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            25,
        )
        .expect("prepare")
        .expect("pending");
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: "first".into(),
            activation_id: id.clone(),
            attempt: prepared.attempt,
            dispatch_identity: prepared.dispatch_identity.clone(),
            receipt: serde_json::json!({"accepted": true}),
            admitted_at_ms: 26,
        })
        .expect("receipt");
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "repair-publication".into(),
        expected_revision: 2,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { edge_id: 0 }],
        reconciliation: vec![bcode_workflow::WorkflowRunGraphReconciliation::Cancel {
            activation_id: id,
        }],
    };
    store
        .stage_run_graph_edit(&request, &authority, 27)
        .expect("stage");
    store
        .accept_pending_run_graph_publication(&run.run_id, &request.mutation_id, &authority, 28)
        .expect("accept");
    block_on(
        update("workflow_attempts")
            .value("status", "repair_required")
            .where_eq("dispatch_identity", prepared.dispatch_identity.as_str())
            .execute(&store.connection),
    )
    .expect("ambiguous attempt");
    let resolution = RepairResolution::ConfirmCancelled {
        message: "operator verified cancellation".into(),
    };
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(
        store
            .repair_attempt_owned(&prepared.dispatch_identity, &resolution, 29, &stale)
            .is_err()
    );
    store
        .repair_attempt_owned(&prepared.dispatch_identity, &resolution, 30, &authority)
        .expect("explicit repair");
    let result = store
        .finalize_pending_run_graph_publication(&run.run_id, &request.mutation_id, &authority, 31)
        .expect("finalize");
    assert!(matches!(
        result,
        bcode_workflow::WorkflowRunGraphPublicationStatus::Committed { revision: 3 }
    ));
}
#[test]
fn run_summary_rejects_invalid_authorization_identity_without_mutation() {
    for (field, value) in [
        ("version", serde_json::json!(2)),
        ("provider_id", serde_json::json!("")),
        ("profile_id", serde_json::json!("")),
        ("policy_digest_sha256", serde_json::json!("invalid")),
    ] {
        let (_temp, store) = initialized_store();
        let mut profile = serde_json::to_value(new_run().authorization_profile).expect("profile");
        profile[field] = value;
        block_on(
            update("workflow_runs")
                .value("authorization_profile_json", profile.to_string())
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("invalid identity fixture");
        let before = fixture_changes(&store.connection);
        assert!(matches!(
            store.run_summary("run-1"),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}
#[test]
fn run_summary_rejects_oversized_valid_authorization_profile() {
    let (_temp, store) = initialized_store();
    let original: String = authoring_fixture_text(
        &store,
        select("workflow_runs")
            .columns(&["authorization_profile_json"])
            .where_eq("run_id", "run-1"),
        "authorization_profile_json",
    )
    .expect("valid profile JSON");
    let at_limit = format!(
        "{original}{}",
        " ".repeat(MAX_INLINE_JSON_BYTES - original.len())
    );
    block_on(
        update("workflow_runs")
            .value("authorization_profile_json", at_limit.as_str())
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("profile at limit");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .run_summary("run-1")
            .expect("exact-limit summary")
            .is_some()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    let payload = format!("{at_limit} ");
    block_on(
        update("workflow_runs")
            .value("authorization_profile_json", payload.as_str())
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("padded profile");
    let before = fixture_changes(&store.connection);
    assert!(matches!(
        store.run_summary("run-1"),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        update("workflow_runs")
            .value("authorization_profile_json", original.as_str())
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("restore profile fixture");
    assert!(
        store
            .run_summary("run-1")
            .expect("restored summary")
            .is_some()
    );
}
#[test]
fn completed_result_admission_preserves_output_and_checks_publication_deadline() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("sequential", 1, &sequential_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "sequential".into();
    run.run_id = "sequential-run".into();
    store.create_run(&run).expect("run");
    let first_id = activation_identity("sequential-run", "first", 0);
    let output = ValidatedOutput {
        output_id: "completed-first".into(),
        run_id: run.run_id.clone(),
        node_id: "first".into(),
        activation_id: first_id.clone(),
        schema_id: "u32".into(),
        schema_version: 1,
        value: serde_json::json!(2),
        artifact_reference: None,
        created_at_ms: 20,
    };
    store
        .persist_validated_output(&output)
        .expect("complete source");
    seed_completed_connected_publication(&store, &first_id);
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "completed".into(),
        expected_revision: 1,
        edits: vec![],
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: first_id,
                edge_ids: vec![0],
            },
        ],
    };
    assert_ambiguous_completed_admission(&store, &request);
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let error = run_graph::admit_completed_result_successors(transaction.as_ref(), &request, 2, 30)
        .expect_err("publication deadline");
    assert!(error.to_string().contains("deadline"));
    block_on(transaction.rollback()).expect("rollback");
    assert_eq!(
        store
            .activations_for_run(&run.run_id, 10)
            .expect("activations")
            .len(),
        1
    );
    reset_completed_publication_candidate(&store);
    assert_completed_admission_late_failure(&store, &request);
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    run_graph::persist_graph_publication(transaction.as_ref(), &request, &[], 25)
        .expect("publication");
    database::commit(transaction).expect("commit");
    let activations = store
        .activations_for_run(&run.run_id, 10)
        .expect("activations");
    assert_eq!(activations.len(), 2);
    let successor = activations
        .iter()
        .find(|item| item.node_id == "second")
        .expect("successor");
    let input: String = authoring_fixture_text(
        &store,
        select("workflow_activations")
            .columns(&["input_json"])
            .where_eq("node_id", "second"),
        "input_json",
    )
    .expect("input");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&input).expect("json"),
        serde_json::json!(2)
    );
    assert_eq!(
        store
            .activation_admitted_graph_revision(&run.run_id, "second", &successor.activation_id)
            .expect("revision"),
        Some(2)
    );
    assert_eq!(
        store.validated_outputs(&run.run_id, 10).expect("outputs"),
        vec![output]
    );
}
#[test]
fn publication_adds_parallel_controller_and_settles_tuple() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let parallel = parallel_join_definition();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "parallel-added".into(),
        expected_revision: 2,
        edits: parallel_publication_edits(&parallel),
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: activation_identity(&run.run_id, "first", 0),
                edge_ids: vec![0],
            },
        ],
    };
    store
        .stage_run_graph_edit(&request, &authority, 25)
        .expect("stage");
    store
        .publish_retained_leaf_run_graph_edit(&run.run_id, &request.mutation_id, &authority, 26)
        .expect("publish");
    let mut extension = request.clone();
    extension.mutation_id = "preserve-parallel".into();
    extension.expected_revision = 3;
    let mut extra = parallel.nodes["left"].clone();
    extra.id = "extra".into();
    extension.edits = vec![bcode_workflow::WorkflowRunGraphEdit::AddNode {
        node: extra,
        entry: true,
        exit: true,
    }];
    extension
        .reconciliation
        .extend([("left", 1), ("right", 2)].map(|(node, edge)| {
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: activation_identity(&run.run_id, node, 0),
                edge_ids: vec![edge],
            }
        }));
    store
        .stage_run_graph_edit(&extension, &authority, 27)
        .expect("stage preserving controller");
    store
        .publish_retained_leaf_run_graph_edit(&run.run_id, &extension.mutation_id, &authority, 28)
        .expect("preserve controller");
    let mut revised = extension.clone();
    revised.mutation_id = "reorder-planned-parallel".into();
    revised.expected_revision = 4;
    let mut join = parallel.nodes["join"].clone();
    join.configuration["left_exits"] = serde_json::json!(["right"]);
    join.configuration["right_exits"] = serde_json::json!(["left"]);
    revised.edits = vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
        node: join,
        entry: false,
        exit: true,
    }];
    revised
        .reconciliation
        .push(bcode_workflow::WorkflowRunGraphReconciliation::Retain {
            activation_id: activation_identity(&run.run_id, "extra", 0),
        });
    store
        .stage_run_graph_edit(&revised, &authority, 29)
        .expect("stage reordered tuple");
    store
        .publish_retained_leaf_run_graph_edit(&run.run_id, &revised.mutation_id, &authority, 29)
        .expect("publish reordered tuple");
    for (node, value) in [("left", 2), ("right", 3)] {
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: format!("{node}-out"),
                run_id: run.run_id.clone(),
                node_id: node.into(),
                activation_id: activation_identity(&run.run_id, node, 0),
                schema_id: "u32".into(),
                schema_version: 1,
                value: serde_json::json!(value),
                artifact_reference: None,
                created_at_ms: 30,
            })
            .expect("member");
    }
    replace_admitted_parallel_join(&mut store, &run, &authority, &parallel);
    store
        .settle_pending_control_nodes(&run.run_id, 10, 35)
        .expect("join");
    assert_replaced_parallel_history(&store, &run.run_id);
}
#[test]
fn connected_publication_preserves_existing_repeat_controller() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let definition = repeat_definition();
    store
        .persist_definition("repeat-edit", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "repeat-edit".into();
    store.create_run(&run).expect("run");
    let node = definition.nodes["body"].clone();
    let mut added = node;
    added.id = "additional".into();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "repeat-preserved".into(),
        expected_revision: 1,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::AddNode {
            node: added,
            entry: true,
            exit: true,
        }],
        reconciliation: Vec::new(),
    };
    store
        .validate_connected_publication(&request)
        .expect("unchanged repeat permitted");
    for edit in [
        bcode_workflow::WorkflowRunGraphEdit::RemoveNode {
            node_id: "repeat-control".into(),
        },
        bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { edge_id: 1 },
    ] {
        let mut deleted = request.clone();
        deleted.edits.push(edit);
        assert!(store.validate_connected_publication(&deleted).is_err());
    }
    let mut changed = request;
    let mut controller = definition.nodes["repeat-control"].clone();
    controller.configuration["max_iterations"] = serde_json::json!(99);
    changed
        .edits
        .push(bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
            node: controller,
            entry: false,
            exit: true,
        });
    assert!(store.validate_connected_publication(&changed).is_err());
}
#[test]
fn publication_inside_repeat_survives_reopen_and_advances_iteration() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = repeat_definition();
    let mut evaluation = definition.nodes["body"].clone();
    evaluation.id = "evaluation".into();
    definition.nodes.insert(evaluation.id.clone(), evaluation);
    definition.edges[0].to = "evaluation".into();
    definition.edges.push(bcode_workflow::EdgeDefinition {
        from: "evaluation".into(),
        to: "repeat-control".into(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    });
    store
        .persist_definition("example", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.input = Some(serde_json::json!({"condition_met":false,"iteration":1}));
    store.create_run(&run).expect("run");
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("owner");
    let authority = store
        .execution_authority(&run.run_id)
        .expect("authority")
        .expect("owner");
    let body = store
        .pending_activations(10)
        .expect("pending")
        .pop()
        .expect("body");
    let mut added = definition.nodes["body"].clone();
    added.id = "additional".into();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "repeat-transaction".into(),
        expected_revision: 1,
        edits: vec![
            bcode_workflow::WorkflowRunGraphEdit::AddNode {
                node: added,
                entry: false,
                exit: false,
            },
            bcode_workflow::WorkflowRunGraphEdit::ReplaceEdge {
                edge_id: 0,
                edge: bcode_workflow::EdgeDefinition {
                    from: "body".into(),
                    to: "additional".into(),
                    kind: bcode_workflow::EdgeKind::Direct,
                    transform: None,
                },
            },
            bcode_workflow::WorkflowRunGraphEdit::AddEdge {
                edge_id: 3,
                edge: bcode_workflow::EdgeDefinition {
                    from: "additional".into(),
                    to: "evaluation".into(),
                    kind: bcode_workflow::EdgeKind::Direct,
                    transform: None,
                },
            },
        ],
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: body.activation_id.clone(),
                edge_ids: vec![0],
            },
        ],
    };
    store
        .stage_run_graph_edit(&request, &authority, 20)
        .expect("stage");
    store
        .publish_retained_leaf_run_graph_edit(&run.run_id, &request.mutation_id, &authority, 21)
        .expect("publish");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "body-after-edit".into(),
            run_id: run.run_id.clone(),
            node_id: body.node_id,
            activation_id: body.activation_id,
            schema_id: body.node.output.type_name,
            schema_version: 1,
            value: run.input.expect("input"),
            artifact_reference: None,
            created_at_ms: 22,
        })
        .expect("body output");
    settle_inserted_repeat_successors(&mut store, &run.run_id);
}
#[test]
fn connected_publication_executes_replaced_unstarted_successor_after_reopen() {
    let (temp, mut store, run, authority, request) = connected_publication_fixture();
    let first_id = activation_identity(&run.run_id, "first", 0);
    let bcode_workflow::WorkflowRunGraphEdit::ReplaceNode { node: second, .. } = &request.edits[0]
    else {
        panic!("replacement");
    };
    drop(store);
    store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store
            .publish_retained_leaf_run_graph_edit(&run.run_id, &request.mutation_id, &authority, 24)
            .expect("duplicate"),
        2
    );
    assert_eq!(
        store
            .activation_admitted_graph_revision(&run.run_id, "first", &first_id)
            .expect("original admission"),
        Some(1)
    );
    let result = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "first-result".into(),
            run_id: run.run_id.clone(),
            node_id: "first".into(),
            activation_id: first_id,
            schema_id: "u32".into(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 25,
        })
        .expect("settle retained source");
    assert_eq!(result.activated.len(), 1);
    let successor = &result.activated[0];
    assert_eq!(successor.node_id, "second");
    assert_eq!(successor.input, Some(serde_json::json!(2)));
    assert_eq!(
        store
            .activation_admitted_graph_revision(&run.run_id, "second", &successor.activation_id)
            .expect("new admission"),
        Some(2)
    );
    assert_eq!(
        store
            .activation_graph_node(&run.run_id, "second", &successor.activation_id)
            .expect("bound node")
            .map(|record| record.node),
        Some(second.clone())
    );
    let finished = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "second-result".into(),
            run_id: run.run_id.clone(),
            node_id: "second".into(),
            activation_id: successor.activation_id.clone(),
            schema_id: "u32".into(),
            schema_version: 1,
            value: serde_json::json!(3),
            artifact_reference: None,
            created_at_ms: 26,
        })
        .expect("settle new successor");
    assert_eq!(finished.run_status, RunStatus::Completed);
}
#[test]
fn owned_preparation_recovery_rejects_stale_authority_without_terminalizing() {
    let (_temp, mut store, run, mut authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            25,
        )
        .expect("prepare")
        .expect("pending");
    authority.generation += 1;
    assert!(
        store
            .reconcile_owned_prepared_attempts_for_run(&run.run_id, &authority, 10, 26)
            .is_err()
    );
    let status: String = authoring_fixture_text(
        &store,
        select("workflow_attempts")
            .columns(&["status"])
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("terminal_at_ms", DatabaseValue::Null),
        "status",
    )
    .expect("untouched attempt");
    assert_eq!(status, "prepared");
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Running
    );
    authority.generation -= 1;
    assert_eq!(
        store
            .reconcile_owned_prepared_attempts_for_run(&run.run_id, &authority, 10, 27)
            .expect("owner recovery")
            .repair_required
            .len(),
        1
    );
}
#[test]
fn owned_receipt_rejects_stale_authority_without_admitting_attempt() {
    let (_temp, mut store, run, mut authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({}),
            25,
        )
        .expect("prepare")
        .expect("pending");
    store
        .record_dispatch_handoff(&prepared, Some(&authority))
        .expect("handoff");
    authority.generation += 1;
    let receipt = DispatchReceipt {
        run_id: run.run_id.clone(),
        node_id: "first".into(),
        activation_id: id,
        attempt: prepared.attempt,
        dispatch_identity: prepared.dispatch_identity,
        receipt: serde_json::json!({"accepted": true}),
        admitted_at_ms: 26,
    };
    assert!(
        store
            .persist_dispatch_receipt_with_authority(&receipt, Some(&authority))
            .is_err()
    );
    let status: String = authoring_fixture_text(
        &store,
        select("workflow_attempts")
            .columns(&["status"])
            .where_eq("dispatch_identity", receipt.dispatch_identity.as_str())
            .where_eq("receipt_json", DatabaseValue::Null),
        "status",
    )
    .expect("receipt not committed");
    assert_eq!(status, "prepared");
}
#[test]
fn exhaustion_observation_rechecks_authority_and_allowance() {
    let (_temp, store, run, authority, _) = connected_publication_fixture();
    let before = fixture_changes(&store.connection);
    store
        .record_execution_allowance_exhaustion(&run.run_id, Some(&authority), 25)
        .unwrap();
    assert_eq!(fixture_changes(&store.connection), before);
    let mut stale = authority;
    stale.generation += 1;
    assert!(
        store
            .record_execution_allowance_exhaustion(&run.run_id, Some(&stale), 26)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn stale_preparation_authority_creates_no_attempt_or_resources() {
    let (_temp, store, run, mut authority, _) = connected_publication_fixture();
    let pending = store
        .pending_activations_for_run(&run.run_id, 10)
        .expect("pending")
        .remove(0);
    authority.generation += 1;
    assert!(
        store
            .acquire_activation_resources(&pending, 25, Some(&authority))
            .is_err()
    );
    assert!(
        store
            .prepare_pending_activation_with_authority(
                &run.run_id,
                &pending.node_id,
                &pending.activation_id,
                DispatchSideEffect::Mutating,
                serde_json::json!({}),
                25,
                Some(&authority)
            )
            .is_err()
    );
    assert!(
        store
            .attempt_history(&run.run_id, None, 10)
            .expect("attempts")
            .is_empty()
    );
    assert!(
        store
            .resource_leases_for_run(&run.run_id, 10)
            .expect("leases")
            .is_empty()
    );
    assert_eq!(
        store
            .pending_activations_for_run(&run.run_id, 10)
            .expect("still pending")
            .len(),
        1
    );
}
#[test]
fn dispatch_handoff_rejects_changed_execution_authority_without_marking_handoff() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({}),
            25,
        )
        .expect("prepare")
        .expect("pending");
    block_on(
        update("workflow_runs")
            .value(
                "coordinator_generation",
                add(
                    identifier("coordinator_generation"),
                    DatabaseValue::Int64(1),
                ),
            )
            .where_eq("run_id", run.run_id.as_str())
            .execute(&store.connection),
    )
    .expect("ownership changed");
    assert!(
        store
            .record_dispatch_handoff(&prepared, Some(&authority))
            .is_err()
    );
    let handed_off: bool = authoring_fixture_unsigned(
        &store,
        select("workflow_dispatch_handoffs")
            .columns(&["handed_off"])
            .where_eq("dispatch_identity", prepared.dispatch_identity.as_str()),
        "handed_off",
    )
    .map(|value| value != 0)
    .expect("handoff proof");
    assert!(!handed_off);
    assert!(store.record_dispatch_handoff(&prepared, None).is_err());
}
#[test]
fn repair_rechecks_authority_after_transfer_and_preserves_attempt_on_refusal() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            25,
        )
        .expect("prepare")
        .expect("pending");
    store
        .reconcile_prepared_attempts(10, 26)
        .expect("ambiguous attempt");
    let replacement = WorkflowExecutionAuthority {
        daemon_instance_id: "replacement".into(),
        generation: authority.generation + 1,
        fencing_token: "replacement-token".into(),
        ..authority.clone()
    };
    store
        .transfer_execution_authority(&run.run_id, &authority, &replacement, 27)
        .expect("transfer");
    let resolution = RepairResolution::ConfirmCancelled {
        message: "operator verified termination".into(),
    };
    assert!(
        store
            .repair_attempt_owned(&prepared.dispatch_identity, &resolution, 28, &authority)
            .is_err()
    );
    assert_eq!(
        store
            .attempt_by_dispatch_identity(&prepared.dispatch_identity)
            .expect("lookup")
            .expect("attempt")
            .status,
        "repair_required"
    );
    let repaired = store
        .repair_attempt_owned(&prepared.dispatch_identity, &resolution, 29, &replacement)
        .expect("current owner repair");
    assert_eq!(repaired.attempt_status, "cancelled");
}
#[test]
fn authority_transfer_rejects_reused_fence_and_mismatched_end_evidence() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let mut replacement = WorkflowExecutionAuthority {
        daemon_instance_id: "replacement".into(),
        generation: authority.generation + 1,
        ..authority.clone()
    };
    assert!(
        store
            .transfer_execution_authority(&run.run_id, &authority, &replacement, 20)
            .is_err()
    );
    replacement.fencing_token = "new-fence".into();
    let evidence = EndedOwnerEvidence {
        ended_daemon_instance_id: "unrelated-owner".into(),
        ended_target_artifact_id: authority.target_artifact_id.clone(),
        liveness: EndedOwnerLiveness::ObservedEnded,
        artifact_image_available: true,
    };
    assert!(
        store
            .reassign_execution_authority_from_ended_owner(
                &run.run_id,
                &authority,
                &replacement,
                &evidence,
                21,
            )
            .is_err()
    );
    assert_eq!(
        store.execution_authority(&run.run_id).expect("authority"),
        Some(authority)
    );
}
#[test]
fn authority_reads_reject_damaged_identity_without_repair() {
    for (column, value) in [
        ("target_artifact_id", Some("")),
        ("coordinator_daemon_instance_id", Some(" ")),
        ("coordinator_fencing_token", Some("")),
        ("target_artifact_id", None),
        ("coordinator_daemon_instance_id", None),
        ("coordinator_fencing_token", None),
    ] {
        let (_temp, store, run, authority, _) = connected_publication_fixture();
        let query = update("workflow_runs");
        let query = match value {
            Some(value) => query.value(column, value),
            None => query.value(column, zero_blob(1_000_000)),
        };
        block_on(store.connection.exec_update_count(&query)).expect("damage");
        let before = fixture_changes(&store.connection);
        assert!(store.execution_authority(&run.run_id).is_err());
        assert!(
            store
                .verify_execution_authority(&run.run_id, &authority)
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
}
#[test]
fn authority_transfer_rejects_empty_replacement_identity_without_writes() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    for (daemon, fence) in [("", "new-fence"), ("new-daemon", ""), (" ", "new-fence")] {
        let replacement = WorkflowExecutionAuthority {
            daemon_instance_id: daemon.into(),
            fencing_token: fence.into(),
            generation: authority.generation + 1,
            ..authority.clone()
        };
        let before = fixture_changes(&store.connection);
        assert!(
            store
                .transfer_execution_authority(&run.run_id, &authority, &replacement, 20,)
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), before);
        assert_eq!(
            store.execution_authority(&run.run_id).expect("owner"),
            Some(authority.clone())
        );
    }
}
#[test]
fn cancellation_intent_rejects_stale_authority_without_mutation() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(
        store
            .request_cancellation_owned(&run.run_id, 25, &stale)
            .is_err()
    );
    assert!(!cancellation_requested_for_run(&store.connection, &run.run_id).expect("intent"));
    assert!(
        store
            .request_cancellation_owned(&run.run_id, 26, &authority)
            .expect("owned intent")
    );
    assert!(
        !store
            .request_cancellation_owned(&run.run_id, 27, &authority)
            .expect("duplicate")
    );
}
#[test]
fn cancellation_settlement_rejects_stale_caller_authority() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({}),
            25,
        )
        .expect("prepare")
        .expect("pending");
    store.request_cancellation(&run.run_id, 26).expect("cancel");
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(
        store
            .mark_sibling_cancellation_signalled_owned(&prepared.dispatch_identity, 27, &stale,)
            .is_err()
    );
    assert!(
        store
            .mark_cancellation_signalled_owned(&prepared.dispatch_identity, 27, &stale)
            .is_err()
    );
    assert!(
        store
            .settle_orphaned_attempt_cancellation_owned(&prepared.dispatch_identity, 27, &stale)
            .is_err()
    );
    assert_eq!(
        store
            .active_attempt_cancellations(&run.run_id, 10)
            .expect("pending")
            .len(),
        1
    );
    assert!(
        store
            .mark_cancellation_signalled_owned(&prepared.dispatch_identity, 28, &authority)
            .expect("signal")
    );
    assert_eq!(
        store
            .settle_orphaned_attempt_cancellation_owned(&prepared.dispatch_identity, 29, &authority)
            .expect("settle"),
        RunStatus::Cancelled
    );
}
#[test]
fn publication_cancellation_serializes_with_durable_dispatch_handoff() {
    for handed_off in [false, true] {
        let (_temp, mut store, run, authority, _) = connected_publication_fixture();
        let id = activation_identity(&run.run_id, "first", 0);
        let prepared = store
            .prepare_pending_activation(
                &run.run_id,
                "first",
                &id,
                DispatchSideEffect::Mutating,
                serde_json::json!({"operation": "test"}),
                25,
            )
            .expect("prepare")
            .expect("pending");
        if handed_off {
            store
                .record_dispatch_handoff(&prepared, Some(&authority))
                .expect("handoff");
        }
        let request = bcode_workflow::WorkflowRunGraphEditBatch {
            version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
            run_id: run.run_id.clone(),
            mutation_id: "cancel-prepared".into(),
            expected_revision: 2,
            edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { edge_id: 0 }],
            reconciliation: vec![bcode_workflow::WorkflowRunGraphReconciliation::Cancel {
                activation_id: id,
            }],
        };
        store
            .stage_run_graph_edit(&request, &authority, 26)
            .expect("stage");
        let result = store.publish_retained_leaf_run_graph_edit(
            &run.run_id,
            &request.mutation_id,
            &authority,
            27,
        );
        if handed_off {
            assert!(result.is_err());
            assert_eq!(
                store.run_graph_revision(&run.run_id).expect("revision"),
                Some(2)
            );
        } else {
            assert_eq!(result.expect("cancel before handoff"), 3);
            assert!(
                store
                    .record_dispatch_handoff(&prepared, Some(&authority))
                    .is_err()
            );
            assert!(
                store
                    .persist_dispatch_receipt(&DispatchReceipt {
                        run_id: run.run_id,
                        node_id: "first".into(),
                        activation_id: prepared.activation.activation_id,
                        attempt: prepared.attempt,
                        dispatch_identity: prepared.dispatch_identity,
                        receipt: serde_json::json!({"accepted": true}),
                        admitted_at_ms: 28,
                    })
                    .is_err()
            );
        }
    }
}
#[test]
fn connected_publication_retains_admitted_target_without_rebinding() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let first_id = activation_identity(&run.run_id, "first", 0);
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "first-result".into(),
            run_id: run.run_id.clone(),
            node_id: "first".into(),
            activation_id: first_id,
            schema_id: "u32".into(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 25,
        })
        .expect("admit second");
    let second_id = activation_identity(&run.run_id, "second", 0);
    let mut third = store
        .current_run_graph_node(&run.run_id, "second")
        .expect("node")
        .expect("second")
        .node;
    third.id = "third".into();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "extend-active-chain".into(),
        expected_revision: 2,
        edits: vec![
            bcode_workflow::WorkflowRunGraphEdit::AddNode {
                node: third,
                entry: false,
                exit: true,
            },
            bcode_workflow::WorkflowRunGraphEdit::AddEdge {
                edge_id: 1,
                edge: bcode_workflow::EdgeDefinition {
                    from: "second".into(),
                    to: "third".into(),
                    kind: bcode_workflow::EdgeKind::Direct,
                    transform: None,
                },
            },
        ],
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: second_id.clone(),
                edge_ids: vec![1],
            },
        ],
    };
    store
        .stage_run_graph_edit(&request, &authority, 26)
        .expect("stage");
    assert_eq!(
        store
            .publish_retained_leaf_run_graph_edit(&run.run_id, &request.mutation_id, &authority, 27)
            .expect("retain admitted successor"),
        3
    );
    assert_eq!(
        store
            .activation_admitted_graph_revision(&run.run_id, "second", &second_id)
            .expect("historical admission"),
        Some(2)
    );
    let result = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "second-result".into(),
            run_id: run.run_id.clone(),
            node_id: "second".into(),
            activation_id: second_id,
            schema_id: "u32".into(),
            schema_version: 1,
            value: serde_json::json!(3),
            artifact_reference: None,
            created_at_ms: 28,
        })
        .expect("settle retained target");
    assert_eq!(result.activated.len(), 1);
    assert_eq!(result.activated[0].node_id, "third");
    assert_eq!(result.activated[0].input, Some(serde_json::json!(3)));
}
#[test]
fn connected_publication_rejects_readmitting_a_historical_target() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let first_id = activation_identity(&run.run_id, "first", 0);
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "first-result".into(),
            run_id: run.run_id.clone(),
            node_id: "first".into(),
            activation_id: first_id.clone(),
            schema_id: "u32".into(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 25,
        })
        .expect("admit second");
    let second_id = activation_identity(&run.run_id, "second", 0);
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "cancel-target".into(),
        expected_revision: 2,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceEdge {
            edge_id: 0,
            edge: bcode_workflow::EdgeDefinition {
                from: "first".into(),
                to: "second".into(),
                kind: bcode_workflow::EdgeKind::Direct,
                transform: None,
            },
        }],
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: first_id,
                edge_ids: vec![0],
            },
            bcode_workflow::WorkflowRunGraphReconciliation::Cancel {
                activation_id: second_id,
            },
        ],
    };
    store
        .stage_run_graph_edit(&request, &authority, 26)
        .expect("stage");
    assert!(
        store
            .publish_retained_leaf_run_graph_edit(&run.run_id, &request.mutation_id, &authority, 27)
            .is_err()
    );
    assert_eq!(
        store.run_graph_revision(&run.run_id).expect("revision"),
        Some(2)
    );
    assert_eq!(store.pending_activations(10).expect("pending").len(), 1);
}
#[test]
fn outcome_collection_does_not_admit_controller_after_publication_retirement() {
    let temp = tempfile::tempdir().expect("temp");
    let definition = outcome_collection_definition();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("retirement", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "retirement".into();
    store.create_run(&run).expect("run");
    let attempt = store
        .prepare_pending_activation(
            &run.run_id,
            "left",
            &activation_identity(&run.run_id, "left", 0),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({}),
            2,
        )
        .expect("prepare")
        .expect("attempt");
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: "left".into(),
            activation_id: attempt.activation.activation_id,
            attempt: attempt.attempt,
            dispatch_identity: attempt.dispatch_identity.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 3,
        })
        .expect("receipt");
    // State after accepted graph-edit retirement, before sibling settlement.
    block_on(async {
        insert("workflow_graph_edit_candidates")
            .value("run_id", "run-1")
            .value("mutation_id", "retire")
            .value("expected_revision", 1)
            .value("request_json", "{}")
            .value("authority_json", "{}")
            .value("created_at_ms", 4)
            .execute(&store.connection)
            .await
            .expect("candidate");
        insert("workflow_pending_publications")
            .value("run_id", "run-1")
            .value("mutation_id", "retire")
            .value("expected_revision", 1)
            .value("accepted_at_ms", 4)
            .execute(&store.connection)
            .await
            .expect("pending");
        update("workflow_attempts")
            .value("status", "cancelled")
            .execute(&store.connection)
            .await
            .expect("attempt");
        update("workflow_activations")
            .value("status", "cancelled")
            .where_eq("node_id", "left")
            .execute(&store.connection)
            .await
            .expect("activation");
        insert("workflow_publication_cancellations")
            .value("run_id", run.run_id.as_str())
            .value("mutation_id", "retire")
            .value("dispatch_identity", attempt.dispatch_identity.as_str())
            .execute(&store.connection)
            .await
            .expect("cancellation intent");
    });
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "surviving-contribution".into(),
            run_id: run.run_id.clone(),
            node_id: "right".into(),
            activation_id: activation_identity(&run.run_id, "right", 0),
            schema_id: definition.nodes["right"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 5,
        })
        .expect("preserve contribution");
    assert!(
        !store
            .pending_activations(10)
            .expect("pending")
            .iter()
            .any(|item| item.node_id == "join")
    );
    let outputs = store.output_summaries(&run.run_id, 10).expect("outputs");
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].node_id, "right");
}
#[test]
fn authored_list_keyset_pagination_is_stable_and_complete() {
    let temp = tempfile::tempdir().expect("temp");
    let (workflow, draft, _) = authored_store_fixture();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    for index in 0..3_u64 {
        let mut item = workflow.clone();
        item.workflow_id = format!("authored/{index}");
        item.title = format!("Workflow {index}");
        item.created_at_ms = index + 1;
        item.updated_at_ms = if index < 2 { 10 } else { 5 };
        assert!(store.create_authored_workflow(&item).expect("workflow"));
    }

    let first = store
        .list_authored_workflows_page(None, 2)
        .expect("first page");
    assert!(first.has_more);
    assert_eq!(
        first
            .items
            .iter()
            .map(|workflow| workflow.workflow_id.as_str())
            .collect::<Vec<_>>(),
        vec!["authored/0", "authored/1"]
    );
    let cursor = WorkflowAuthoringListCursor {
        updated_at_ms: first.items[1].updated_at_ms,
        entity_id: first.items[1].workflow_id.clone(),
    };
    let second = store
        .list_authored_workflows_page(Some(&cursor), 2)
        .expect("second page");
    assert!(!second.has_more);
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].workflow_id, "authored/2");

    let mut first_draft = draft;
    first_draft.workflow_id = "authored/0".to_string();
    first_draft.document.workflow_id = first_draft.workflow_id.clone();
    first_draft.checksum_sha256 = first_draft
        .document
        .source_digest_sha256()
        .expect("draft checksum");
    assert!(store.create_workflow_draft(&first_draft).expect("draft"));
    let mut second_draft = first_draft.clone();
    second_draft.draft_id = "draft-2".to_string();
    assert!(store.create_workflow_draft(&second_draft).expect("draft"));
    let drafts = store
        .list_workflow_drafts_page("authored/0", None, 1)
        .expect("draft page");
    assert!(drafts.has_more);
    assert_eq!(drafts.items[0].draft_id, "draft-1");
    let cursor = WorkflowAuthoringListCursor {
        updated_at_ms: drafts.items[0].updated_at_ms,
        entity_id: drafts.items[0].draft_id.clone(),
    };
    let drafts = store
        .list_workflow_drafts_page("authored/0", Some(&cursor), 1)
        .expect("next draft page");
    assert!(!drafts.has_more);
    assert_eq!(drafts.items[0].draft_id, "draft-2");
}
#[test]
fn authored_inspection_is_bounded_non_mutating_and_reports_corruption() {
    let temp = tempfile::tempdir().expect("temp");
    let (workflow, draft, catalog) = authored_store_fixture();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store.create_authored_workflow(&workflow).expect("workflow");
    store.create_workflow_draft(&draft).expect("draft");
    let preview = draft.document.compilation_preview(&catalog, None);
    store
        .publish_workflow_draft(
            &workflow.workflow_id,
            &draft.draft_id,
            1,
            &preview,
            true,
            None,
            3,
        )
        .expect("publish");
    let events = store
        .workflow_authoring_events(&workflow.workflow_id, None, 1)
        .expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "revision_published");
    let before = store
        .workflow_revision(&workflow.workflow_id, 1)
        .expect("revision")
        .expect("revision");
    block_on(store.connection.sqlite_foreign_keys(false)).expect("foreign keys");
    block_on(
        update("authored_workflows")
            .value("active_revision", 99)
            .where_eq("workflow_id", workflow.workflow_id.as_str())
            .execute(&store.connection),
    )
    .expect("corrupt active pointer");
    let issues = store
        .diagnose_authored_workflow(&workflow.workflow_id, 1)
        .expect("diagnostics");
    assert_eq!(
        issues,
        vec![WorkflowAuthoringIssue::InvalidActiveRevision { revision: 99 }]
    );
    assert_eq!(
        store
            .workflow_revision(&workflow.workflow_id, 1)
            .expect("revision")
            .expect("revision"),
        before
    );
}
#[test]
fn authored_workflow_draft_revision_and_preset_lifecycle_survives_restart() {
    let temp = tempfile::tempdir().expect("temp");
    let (workflow, draft, catalog) = authored_store_fixture();
    let publication = {
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        assert!(store.create_authored_workflow(&workflow).expect("workflow"));
        assert!(
            !store
                .create_authored_workflow(&workflow)
                .expect("idempotent")
        );
        assert!(store.create_workflow_draft(&draft).expect("draft"));
        let preview = draft.document.compilation_preview(&catalog, None);
        let publication = store
            .publish_workflow_draft(
                &workflow.workflow_id,
                &draft.draft_id,
                1,
                &preview,
                true,
                None,
                3,
            )
            .expect("publication");
        assert_eq!(publication.revision.revision, 1);
        assert_eq!(publication.active_revision, Some(1));
        assert!(
            store
                .workflow_draft(&workflow.workflow_id, &draft.draft_id)
                .expect("draft")
                .is_none()
        );
        let preset = WorkflowPreset {
            workflow_id: workflow.workflow_id.clone(),
            preset_id: "default".to_string(),
            revision: 1,
            name: "Default".to_string(),
            generation: 1,
            configuration: serde_json::json!({"message": "review"}),
            run_limits: Some(WorkflowRunLimitPolicy::default()),
            producer: draft.producer.clone(),
            created_at_ms: 4,
            updated_at_ms: 4,
        };
        assert!(store.create_workflow_preset(&preset).expect("preset"));
        let updated = store
            .update_workflow_preset(
                &workflow.workflow_id,
                "default",
                1,
                "Updated",
                &serde_json::json!({"message": "updated"}),
                None,
                &draft.producer,
                5,
            )
            .expect("updated preset");
        assert_eq!(updated.generation, 2);
        publication
    };

    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let stored = reopened
        .authored_workflow(&workflow.workflow_id)
        .expect("workflow")
        .expect("workflow");
    assert_eq!(stored.active_revision, Some(1));
    assert_eq!(
        reopened
            .workflow_revision(&workflow.workflow_id, 1)
            .expect("revision")
            .expect("revision"),
        publication.revision
    );
    assert_eq!(
        reopened
            .list_workflow_revisions(&workflow.workflow_id, 10)
            .expect("revisions")
            .len(),
        1
    );
    let preset = reopened
        .workflow_preset(&workflow.workflow_id, "default")
        .expect("preset")
        .expect("preset");
    assert_eq!(preset.generation, 2);
    assert_eq!(preset.revision, 1);
    reopened
        .delete_workflow_preset(&workflow.workflow_id, "default", 2)
        .expect("delete preset");
    assert!(
        reopened
            .list_workflow_presets(&workflow.workflow_id, 10)
            .expect("presets")
            .is_empty()
    );
}
#[test]
fn authored_draft_and_active_pointer_conflicts_fail_closed() {
    let temp = tempfile::tempdir().expect("temp");
    let (workflow, draft, catalog) = authored_store_fixture();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store.create_authored_workflow(&workflow).expect("workflow");
    store.create_workflow_draft(&draft).expect("draft");
    let updated = store
        .update_workflow_draft(
            &workflow.workflow_id,
            &draft.draft_id,
            1,
            &draft.document,
            &draft.producer,
            3,
        )
        .expect("update");
    assert_eq!(updated.generation, 2);
    assert!(matches!(
        store.update_workflow_draft(
            &workflow.workflow_id,
            &draft.draft_id,
            1,
            &draft.document,
            &draft.producer,
            4,
        ),
        Err(WorkflowStoreError::AuthoringConflict { .. })
    ));
    let preview = updated.document.compilation_preview(&catalog, None);
    store
        .publish_workflow_draft(
            &workflow.workflow_id,
            &draft.draft_id,
            2,
            &preview,
            true,
            None,
            5,
        )
        .expect("publish");
    let fork = store
        .fork_workflow_revision(
            &workflow.workflow_id,
            1,
            "draft-2",
            draft.producer.clone(),
            6,
        )
        .expect("fork");
    assert_eq!(fork.base_revision, Some(1));
    let preview = fork.document.compilation_preview(&catalog, None);
    let second = store
        .publish_workflow_draft(
            &workflow.workflow_id,
            "draft-2",
            1,
            &preview,
            false,
            None,
            7,
        )
        .expect("second revision");
    assert_eq!(second.revision.revision, 2);
    assert!(matches!(
        store.set_active_workflow_revision(&workflow.workflow_id, None, 2, 8),
        Err(WorkflowStoreError::AuthoringConflict { .. })
    ));
    store
        .set_active_workflow_revision(&workflow.workflow_id, Some(1), 2, 8)
        .expect("activate revision 2");
    store
        .set_authored_workflow_archived(&workflow.workflow_id, true, 9)
        .expect("archive");
    assert!(
        store
            .set_active_workflow_revision(&workflow.workflow_id, Some(2), 1, 10)
            .is_err()
    );
    assert_eq!(
        store
            .list_workflow_revisions(&workflow.workflow_id, 10)
            .expect("revisions")
            .len(),
        2
    );
}
#[test]
fn workflow_presets_reject_secret_bearing_configuration() {
    let temp = tempfile::tempdir().expect("temp");
    let (workflow, draft, catalog) = authored_store_fixture();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store.create_authored_workflow(&workflow).expect("workflow");
    store.create_workflow_draft(&draft).expect("draft");
    let preview = draft.document.compilation_preview(&catalog, None);
    store
        .publish_workflow_draft(
            &workflow.workflow_id,
            &draft.draft_id,
            1,
            &preview,
            true,
            None,
            3,
        )
        .expect("publish");
    for configuration in [
        serde_json::json!({"password": "must-not-persist"}),
        serde_json::json!({"token": {"backend": "sshenv", "profile": "vault"}}),
    ] {
        let preset = WorkflowPreset {
            workflow_id: workflow.workflow_id.clone(),
            preset_id: "unsafe".to_string(),
            revision: 1,
            name: "Unsafe".to_string(),
            generation: 1,
            configuration,
            run_limits: None,
            producer: draft.producer.clone(),
            created_at_ms: 4,
            updated_at_ms: 4,
        };
        assert!(store.create_workflow_preset(&preset).is_err());
        assert!(
            store
                .workflow_preset(&workflow.workflow_id, "unsafe")
                .expect("preset")
                .is_none()
        );
    }
}
#[test]
fn draft_and_preset_update_faults_roll_back_generations() {
    struct Fault;
    impl WorkflowAuthoringMutationFault for Fault {
        fn after_boundary(
            &self,
            _boundary: WorkflowAuthoringMutationBoundary,
        ) -> Result<(), WorkflowStoreError> {
            Err(WorkflowStoreError::InvalidData("fault".to_string()))
        }
    }

    let temp = tempfile::tempdir().expect("temp");
    let (workflow, draft, catalog) = authored_store_fixture();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store.create_authored_workflow(&workflow).expect("workflow");
    store.create_workflow_draft(&draft).expect("draft");
    store
        .update_workflow_draft_with_fault(
            &workflow.workflow_id,
            &draft.draft_id,
            1,
            &draft.document,
            &draft.producer,
            3,
            &Fault,
        )
        .expect_err("draft fault");
    assert_eq!(
        store
            .workflow_draft(&workflow.workflow_id, &draft.draft_id)
            .expect("draft")
            .expect("draft")
            .generation,
        1
    );
    let preview = draft.document.compilation_preview(&catalog, None);
    store
        .publish_workflow_draft(
            &workflow.workflow_id,
            &draft.draft_id,
            1,
            &preview,
            true,
            None,
            4,
        )
        .expect("publish");
    let preset = WorkflowPreset {
        workflow_id: workflow.workflow_id.clone(),
        preset_id: "default".to_string(),
        revision: 1,
        name: "Default".to_string(),
        generation: 1,
        configuration: serde_json::json!({"message": "review"}),
        run_limits: None,
        producer: draft.producer.clone(),
        created_at_ms: 5,
        updated_at_ms: 5,
    };
    store.create_workflow_preset(&preset).expect("preset");
    store
        .update_workflow_preset_with_fault(
            &workflow.workflow_id,
            &preset.preset_id,
            1,
            "Changed",
            &serde_json::json!({"message": "changed"}),
            None,
            &draft.producer,
            6,
            &Fault,
        )
        .expect_err("preset fault");
    assert_eq!(
        store
            .workflow_preset(&workflow.workflow_id, &preset.preset_id)
            .expect("preset")
            .expect("preset"),
        preset
    );
}
#[test]
fn publication_faults_roll_back_every_authoring_boundary() {
    struct Fault(WorkflowPublicationBoundary);
    impl WorkflowPublicationFault for Fault {
        fn after_boundary(
            &self,
            boundary: WorkflowPublicationBoundary,
        ) -> Result<(), WorkflowStoreError> {
            if boundary == self.0 {
                Err(WorkflowStoreError::InvalidData(format!(
                    "fault at {boundary:?}"
                )))
            } else {
                Ok(())
            }
        }
    }

    for boundary in [
        WorkflowPublicationBoundary::DefinitionPersisted,
        WorkflowPublicationBoundary::RevisionPersisted,
        WorkflowPublicationBoundary::ActiveRevisionUpdated,
        WorkflowPublicationBoundary::EventPersisted,
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let (workflow, draft, catalog) = authored_store_fixture();
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store.create_authored_workflow(&workflow).expect("workflow");
        store.create_workflow_draft(&draft).expect("draft");
        let preview = draft.document.compilation_preview(&catalog, None);
        store
            .publish_workflow_draft_with_fault(
                &workflow.workflow_id,
                &draft.draft_id,
                1,
                &preview,
                true,
                None,
                3,
                &Fault(boundary),
            )
            .expect_err("fault");
        assert!(
            store
                .workflow_revision(&workflow.workflow_id, 1)
                .expect("revision")
                .is_none()
        );
        assert!(
            store
                .workflow_draft(&workflow.workflow_id, &draft.draft_id)
                .expect("draft")
                .is_some()
        );
        assert_eq!(
            store
                .authored_workflow(&workflow.workflow_id)
                .expect("workflow")
                .expect("workflow")
                .active_revision,
            None
        );
        let definition_count = authoring_fixture_unsigned(
            &store,
            select("workflow_definitions").project_as(count_all(), "count"),
            "count",
        )
        .expect("definition count");
        let event_count = authoring_fixture_unsigned(
            &store,
            select("workflow_authoring_events").project_as(count_all(), "count"),
            "count",
        )
        .expect("event count");
        assert_eq!(definition_count, 0);
        assert_eq!(event_count, 0);
    }
}

fn authoring_fixture_text(
    store: &WorkflowStore,
    query: SelectQuery<'_>,
    column: &str,
) -> Result<String, WorkflowStoreError> {
    let row = fixture_rows(&store.connection, &query.limit(1))
        .into_iter()
        .next()
        .ok_or(switchy_database::DatabaseError::NoRow)?;
    Ok(fixture_text(&row, column))
}

fn fixture_activation(store: &WorkflowStore) -> Result<(String, String), WorkflowStoreError> {
    let row = database::rows(
        &store.connection,
        &select("workflow_activations")
            .columns(&["node_id", "activation_id"])
            .where_eq("run_id", "run-1")
            .limit(1),
    )?
    .into_iter()
    .next()
    .ok_or(switchy_database::DatabaseError::NoRow)?;
    Ok((
        database::text(&row, "node_id")?,
        database::text(&row, "activation_id")?,
    ))
}

fn authoring_fixture_unsigned(
    store: &WorkflowStore,
    query: SelectQuery<'_>,
    column: &str,
) -> Result<u64, WorkflowStoreError> {
    let row = fixture_rows(&store.connection, &query.limit(1))
        .into_iter()
        .next()
        .ok_or(switchy_database::DatabaseError::NoRow)?;
    Ok(fixture_unsigned(&row, column))
}

fn seed_unstarted_leaf_owner(store: &WorkflowStore) {
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .execute(&store.connection),
    )
    .expect("owner");
}

fn seed_completed_connected_publication(store: &WorkflowStore, first_id: &str) {
    // Model a committed connected graph before successor admission; public publication stays guarded.
    block_on(async {
        delete("workflow_activation_graph_bindings")
            .where_eq("node_id", "second")
            .execute(&store.connection)
            .await
            .expect("bindings");
        delete("workflow_activations")
            .where_eq("node_id", "second")
            .execute(&store.connection)
            .await
            .expect("activation");
        update("workflow_run_graphs")
            .value("revision", 2)
            .execute(&store.connection)
            .await
            .expect("revision");
        update("workflow_runs")
            .value("deadline_at_ms", 30)
            .execute(&store.connection)
            .await
            .expect("deadline");
        insert("workflow_graph_edit_candidates")
            .value("run_id", "sequential-run")
            .value("mutation_id", "completed")
            .value("expected_revision", 1)
            .value("request_json", "{}")
            .value("authority_json", "{}")
            .value("created_at_ms", 1)
            .execute(&store.connection)
            .await
            .expect("candidate");
        insert("workflow_graph_edit_publications")
            .value("run_id", "sequential-run")
            .value("mutation_id", "completed")
            .value("revision", 2)
            .execute(&store.connection)
            .await
            .expect("publication");
        insert("workflow_retained_edge_bindings")
            .value("run_id", "sequential-run")
            .value("mutation_id", "completed")
            .value("graph_revision", 2)
            .value("edge_id", 0)
            .value("edge_revision", 1)
            .value("source_node_id", "first")
            .value("source_activation_id", first_id)
            .execute(&store.connection)
            .await
            .expect("binding");
    });
}

fn install_cancel_publication_fault(store: &WorkflowStore) {
    block_on(
        create_trigger(
            "reject_cancel_publication",
            "workflow_graph_edit_publications",
        )
        .temporary(true)
        .before_insert()
        .raise_abort("publication fault")
        .execute(&store.connection),
    )
    .expect("publication fault");
}

fn reset_completed_publication_candidate(store: &WorkflowStore) {
    block_on(async {
        delete("workflow_retained_edge_bindings")
            .execute(&store.connection)
            .await
            .expect("bindings");
        delete("workflow_graph_edit_publications")
            .execute(&store.connection)
            .await
            .expect("publications");
        update("workflow_run_graphs")
            .value("revision", 1)
            .execute(&store.connection)
            .await
            .expect("unpublished candidate");
    });
}
