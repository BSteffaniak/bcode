use super::*;
use switchy_database::{Database, query::FilterableQuery};

#[tokio::test]
async fn typed_output_page_rejects_damage_and_invalid_limits() {
    let root = tempfile::tempdir().unwrap();
    let store =
        bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path()).unwrap();
    let path = store.path().to_path_buf();
    let mut state = crate::tests::test_server_state(crate::SessionManager::default());
    state.workflow_store = std::sync::Arc::new(std::sync::Mutex::new(store));
    let db = open(&path, true).unwrap();
    representation::install(&db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "outputs",
        bcode_workflow::Step::task("worker", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("outputs", &definition).unwrap();
    let run = bcode_workflow_store::NewWorkflowRun {
        run_id: "outputs".into(),
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
            provider_id: "test".into(),
            profile_id: "build".into(),
            policy_digest_sha256: "a".repeat(64),
        },
        authorization_ceiling: bcode_workflow::WorkflowToolCapability::ReadOnly,
        limits: bcode_workflow::WorkflowRunLimits::default(),
    };
    representation::admit(&db, &run, &definition).await.unwrap();
    for id in ["a", "b"] {
        let output = bcode_workflow_store::ValidatedOutput {
            output_id: id.into(),
            run_id: run.run_id.clone(),
            node_id: "worker".into(),
            activation_id: id.into(),
            schema_id: "u32".into(),
            schema_version: 1,
            value: serde_json::json!(1),
            artifact_reference: None,
            created_at_ms: 2,
        };
        db.insert("workflow_outputs_v2")
            .value("output_id", id)
            .value("run_id", "outputs")
            .value("activation_id", id)
            .value("output_json", serde_json::to_string(&output).unwrap())
            .execute(&db)
            .await
            .unwrap();
    }
    let request = bcode_workflow_view_models::WorkflowCatalogRequest {
        limit: 31,
        cursor: None,
        filter: bcode_workflow_view_models::WorkflowCatalogFilter::default(),
        sort: bcode_workflow_view_models::WorkflowCatalogSort::default(),
        group: bcode_workflow_view_models::WorkflowCatalogGroup::default(),
        search: None,
    };
    let basic = super::super::catalog::basic_view(&state, &request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(basic.runs.len(), 1);
    assert_eq!(basic.runs[0].run_id, "outputs");
    assert!(!basic.basic_discovery.unwrap().progress_available);
    assert_basic_inspection(&state).await;
    let page = run_outputs(&state, "outputs", 1).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].output_id, "a");
    assert_eq!(
        page[0].checksum_sha256,
        bcode_workflow::workflow_canonical_value_sha256(&serde_json::json!(1)).unwrap()
    );
    assert!(run_outputs(&state, "outputs", 0).await.is_err());
    assert!(run_outputs(&state, "outputs", 101).await.is_err());
    db.update("workflow_outputs_v2")
        .value("output_json", "broken")
        .where_eq("output_id", "a")
        .execute(&db)
        .await
        .unwrap();
    let damaged = run_outputs(&state, "outputs", 1).await;
    drop(state);
    assert!(damaged.is_err());
}

async fn assert_basic_inspection(state: &ServerState) {
    let inspected = super::super::read_view::run_view(state, "outputs", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inspected.outputs.len(), 1);
    assert_eq!(inspected.nodes.len(), 1);
    assert_eq!(inspected.nodes[0].node_id, "worker");
    assert!(matches!(
        inspected.nodes[0].status,
        bcode_workflow_view_models::WorkflowNodeStatus::Unknown(_)
    ));
    assert!(inspected.actions.is_empty());
    let scheduling = inspected.scheduling.as_ref().unwrap();
    assert_eq!(scheduling.activations.len(), 1);
    assert_eq!(scheduling.policy.as_ref().unwrap().max_running, None);
    assert_ne!(
        scheduling.activations[0].state,
        bcode_workflow_view_models::WorkflowSchedulingState::Ready
    );
    assert!(matches!(
        inspected.health,
        bcode_workflow_view_models::WorkflowProjectionHealth::Degraded { .. }
    ));
}

#[test]
fn inspection_preserves_exact_identity_and_domain_checksum() {
    let value = serde_json::json!({"unicode":"界", "nested":[1, true, null]});
    let expected = bcode_workflow::workflow_canonical_value_sha256(&value).unwrap();
    let inspected = inspect(bcode_workflow_store::ValidatedOutput {
        output_id: "output".into(),
        run_id: "run".into(),
        node_id: "worker".into(),
        activation_id: "activation".into(),
        schema_id: "schema".into(),
        schema_version: 2,
        value: value.clone(),
        artifact_reference: Some("artifact".into()),
        created_at_ms: 42,
    })
    .unwrap();
    assert_eq!(inspected.checksum_sha256, expected);
    assert_eq!(inspected.value, value);
    assert_eq!(inspected.activation_id, "activation");
    assert_eq!(inspected.artifact_reference.as_deref(), Some("artifact"));
    assert_eq!(inspected.schema_version, 2);
    assert_eq!(inspected.created_at_ms, 42);
}

#[tokio::test]
async fn absent_legacy_outputs_remain_empty_without_installing_extension() {
    let root = tempfile::tempdir().unwrap();
    let store = bcode_workflow_store::WorkflowStore::open_in_state_dir(root.path()).unwrap();
    let path = store.path().to_path_buf();
    let mut state = crate::tests::test_server_state(crate::SessionManager::default());
    state.workflow_store = std::sync::Arc::new(std::sync::Mutex::new(store));
    let outputs = run_outputs(&state, "absent", 10).await.unwrap();
    drop(state);
    assert!(outputs.is_empty());
    let db = open(&path, false).unwrap();
    assert!(
        !switchy_database::Database::table_exists(&db, "workflow_representation_contract")
            .await
            .unwrap()
    );
}
