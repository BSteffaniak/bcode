use crate::ServerState;
use bcode_workflow_store::representation::{self, attempts, graph};
use bcode_workflow_store::{
    ActivationDispatchOwner, ActivationDispatchPlan, PendingActivation, PreparedActivationDispatch,
    PreparedAttempt, WorkflowExecutionAuthority, WorkflowStoreError,
};

pub struct RejectingOwner;
impl ActivationDispatchOwner for RejectingOwner {
    fn plan<'a>(
        &'a self,
        _: &'a PendingActivation,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Option<ActivationDispatchPlan>, WorkflowStoreError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(None) })
    }
    fn dispatch<'a>(
        &'a self,
        _: &'a PreparedActivationDispatch,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, WorkflowStoreError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Err(WorkflowStoreError::InvalidData(
                "private injected failure".into(),
            ))
        })
    }
}

pub async fn prepare() -> (
    tempfile::TempDir,
    ServerState,
    switchy_database::rusqlite::RusqliteDatabase,
    WorkflowExecutionAuthority,
    PreparedActivationDispatch,
) {
    let root = tempfile::tempdir().unwrap();
    let store =
        bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path()).unwrap();
    let path = store.path().to_path_buf();
    let state = crate::tests::test_server_state_with_fake_provider_and_workflow_store(
        bcode_session::SessionManager::default(),
        store,
    );
    let db = diagnostic_database(&path).await;
    let definition = bcode_workflow::WorkflowBuilder::new(
        "diagnostic",
        bcode_workflow::Step::task("worker", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("diagnostic", &definition)
            .unwrap();
    let run = bcode_workflow_store::NewWorkflowRun {
        run_id: "diagnostic".into(),
        definition_id: identity.definition_id,
        definition_version: identity.definition_version,
        workspace_snapshot: "workspace".into(),
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
    representation::admit(&db, &run, &definition).await.unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: crate::workflow_operations::current_artifact_id(&state),
        daemon_instance_id: state.daemon_status.instance_id.clone(),
        generation: 1,
        fencing_token: "token".into(),
    };
    attempts::claim_authority(&db, &run.run_id, &owner)
        .await
        .unwrap();
    let activation_id = graph::activation_id(&db, &run.run_id, "worker")
        .await
        .unwrap()
        .unwrap();
    let prepared = PreparedAttempt {
        run_id: run.run_id.clone(),
        node_id: "worker".into(),
        activation_id: activation_id.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::Mutating,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch_identity = attempts::prepare(&db, &owner, &prepared).await.unwrap();
    let request = PreparedActivationDispatch {
        activation: PendingActivation {
            run_id: run.run_id,
            node_id: "worker".into(),
            activation_id,
            node: definition.nodes["worker"].clone(),
            input: Some(serde_json::json!(1)),
            dependency_generation: 1,
            created_at_ms: 1,
        },
        attempt: 1,
        dispatch_identity,
        intent: prepared.intent,
    };
    (root, state, db, owner, request)
}

async fn diagnostic_database(
    path: &std::path::Path,
) -> switchy_database::rusqlite::RusqliteDatabase {
    let db = switchy_database_connection::open_sqlite_rusqlite(
        path,
        &switchy_database_connection::SqliteOpenOptions {
            access: switchy_database_connection::SqliteAccess::ReadWrite,
            allow_uri: false,
            busy_timeout: std::time::Duration::ZERO,
            foreign_keys: true,
            connections: std::num::NonZeroUsize::MIN,
        },
    )
    .unwrap();
    representation::install(&db).await.unwrap();
    let tx = switchy_database::Database::begin_transaction_with_mode(
        &db,
        switchy_database::TransactionMode::Immediate,
    )
    .await
    .unwrap();
    representation::admission_observation::install_in(&*tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    db
}
