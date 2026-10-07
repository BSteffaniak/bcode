use super::*;
use crate::ServerError;
use bcode_workflow_store::{
    DispatchReceipt, NewWorkflowRun, PreparedAttempt, WorkflowExecutionAuthority,
    WorkflowExecutionSessionLink, WorkflowStore,
};

#[test]
fn publication_categories_are_typed_and_secret_safe() {
    for (error, expected) in [
        (
            RepresentationError::AuthorityConflict,
            PublicationFailure::Authority,
        ),
        (
            RepresentationError::CapacityUnavailable,
            PublicationFailure::Capacity,
        ),
        (
            RepresentationError::InvalidAdmission,
            PublicationFailure::InvalidAdmission,
        ),
        (
            RepresentationError::MaintenanceRequired,
            PublicationFailure::Maintenance,
        ),
        (
            RepresentationError::UnsupportedLifecycle,
            PublicationFailure::Unsupported,
        ),
        (
            RepresentationError::UnsupportedStore,
            PublicationFailure::Unsupported,
        ),
        (
            RepresentationError::Database(switchy_database::DatabaseError::InvalidSchema(
                "injected-secret-token SQL private-path".into(),
            )),
            PublicationFailure::StorageUnavailable,
        ),
    ] {
        let normalized = normalize(&error);
        assert!(matches!(
            normalized,
            ServerError::WorkflowGraphPublicationFailure(actual) if actual == expected
        ));
        let public = crate::request_error_response(&normalized);
        assert_eq!(public.code, expected.code());
        assert!(!public.message.contains("injected-secret-token"));
        assert!(!public.message.contains("private-path"));
        assert!(public.message.len() < 160);
        match crate::workflow_operation_failure(&normalized) {
            bcode_tool::ToolInvocationServiceResolution::Failed { code, message } => {
                assert_eq!(code, expected.code());
                assert!(!message.contains("injected-secret-token"));
                assert!(!message.contains("private-path"));
                assert!(message.len() < 160);
            }
            other => panic!("expected normalized service failure, got {other:?}"),
        }
    }
    assert!(matches!(
        normalize(&RepresentationError::Database(
            switchy_database::DatabaseError::Busy
        )),
        ServerError::WorkflowStorageBusy
    ));
}

fn publication_admission() -> (NewWorkflowRun, bcode_workflow::WorkflowDefinition) {
    let definition = bcode_workflow::WorkflowBuilder::new(
        "publication-diagnostic",
        bcode_workflow::Step::task("source", |value: u32, _| async move { Ok(value) }).then(
            bcode_workflow::Step::task("end", |value: u32, _| async move { Ok(value) }),
        ),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone();
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "publication-diagnostic",
        &definition,
    )
    .expect("identity");
    let run = NewWorkflowRun {
        run_id: "publication-diagnostic".into(),
        definition_id: identity.definition_id,
        definition_version: identity.definition_version,
        workspace_snapshot: "workspace".into(),
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
        limits: bcode_workflow::WorkflowRunLimits::default(),
    };
    (run, definition)
}

async fn publication_fixture() -> (
    tempfile::TempDir,
    super::super::AuthenticatedExecution,
    switchy_database::rusqlite::RusqliteDatabase,
    bcode_workflow::WorkflowDefinition,
) {
    use bcode_workflow_store::representation::{self, attempts, graph};

    let root = tempfile::tempdir().expect("root");
    let store = WorkflowStore::create_coexistence_baseline(root.path()).expect("store");
    let path = store.path().to_path_buf();
    let db = crate::workflow_representation::open(&path, true).expect("database");
    representation::install(&db).await.expect("representation");
    let (run, definition) = publication_admission();
    representation::admit(&db, &run, &definition)
        .await
        .expect("admit");
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "token".into(),
    };
    attempts::claim_authority(&db, &run.run_id, &owner)
        .await
        .expect("owner");
    let activation = graph::activation_id(&db, &run.run_id, "source")
        .await
        .expect("activation")
        .expect("source");
    let prepared = PreparedAttempt {
        run_id: run.run_id.clone(),
        node_id: "source".into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch = attempts::prepare(&db, &owner, &prepared)
        .await
        .expect("prepare");
    let link = WorkflowExecutionSessionLink::new(
        run.run_id.clone(),
        "source".into(),
        activation.clone(),
        1,
        "execution-session".into(),
        run.workspace_snapshot.clone(),
        2,
    );
    attempts::owner_boundary::persist_execution_session_link(&db, &owner, &link)
        .await
        .expect("link");
    attempts::attach_receipt(
        &db,
        &owner,
        &DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: "source".into(),
            activation_id: activation.clone(),
            attempt: 1,
            dispatch_identity: dispatch,
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .expect("receipt");
    let execution = super::super::AuthenticatedExecution {
        link,
        authority: owner,
        source: definition.nodes["source"].clone(),
        retained: std::sync::Mutex::new(store),
        path,
    };
    (root, execution, db, definition)
}

#[tokio::test]
async fn exact_candidate_is_published_without_rewrite_and_stale_owner_is_classified() {
    use bcode_workflow_store::representation::execution_graph;

    let (_root, execution, db, definition) = publication_fixture().await;
    let mut end = definition.nodes["end"].clone();
    end.name = "changed pending node".into();
    end.kind = bcode_workflow::NodeKind::Agent;
    end.configuration = serde_json::to_value(
        bcode_workflow::WorkflowPromptConfiguration::preserve_input("build", "observe"),
    )
    .expect("configuration");
    let batch = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: execution.link.run_id.clone(),
        expected_revision: 1,
        mutation_id: "candidate".into(),
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
            node: end,
            entry: false,
            exit: true,
        }],
        reconciliation: vec![bcode_workflow::WorkflowRunGraphReconciliation::Retain {
            activation_id: execution.link.activation_id.clone(),
        }],
    };
    assert!(matches!(
        execution.publish(&batch).await,
        Err(ServerError::WorkflowGraphPublicationFailure(
            PublicationFailure::InvalidAdmission
        ))
    ));
    execution_graph::stage_run_graph_edit_from_execution(
        &db,
        &batch,
        &execution.authority,
        &execution.link,
        3,
    )
    .await
    .expect("stage");
    assert_eq!(execution.publish(&batch).await.expect("publish"), 2);
    assert_eq!(
        execution_graph::staged(
            &db,
            &execution.link.run_id,
            "candidate",
            &execution.authority
        )
        .await
        .expect("retained candidate"),
        Some(batch.clone())
    );
    let mut foreign = execution;
    foreign.authority.fencing_token = "stale-token".into();
    for result in [
        foreign.publish(&batch).await.map(|_| ()),
        foreign.accept(&batch).await.map(|_| ()),
    ] {
        assert!(matches!(
            result,
            Err(ServerError::WorkflowGraphPublicationFailure(
                PublicationFailure::Authority
            ))
        ));
    }
}
