use super::*;
use crate::workflow_representation::{open, tests::typed_provider_fixture};
use switchy_database::Database;

#[tokio::test]
async fn snapshot_waits_retain_input_and_missing_tables_remain_unavailable() {
    let (_root, _sessions, state, _) = typed_provider_fixture().await;
    let db = open(&crate::workflow_store_path(&state), true).unwrap();
    let owner = representation::attempts::owner_boundary::execution_authority(&db, "new")
        .await
        .unwrap()
        .unwrap();
    let context = representation::hierarchy::parent_context(&db, &owner, "new")
        .await
        .unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "gates",
        bcode_workflow::Step::<u32, u32>::input("input")
            .then(bcode_workflow::Step::<u32, u32>::approval("approval")),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("gates", &definition).unwrap();
    let mut run = context.admission;
    run.run_id = "gates".into();
    run.definition_id = identity.definition_id;
    run.definition_version = identity.definition_version;
    representation::admit(&db, &run, &definition).await.unwrap();
    let tx = db.begin_transaction().await.unwrap();
    let (waits, complete) = inspect(&*tx, "gates", 1, 100).await;
    assert_eq!(complete, Some(true));
    assert_eq!(waits.len(), 1);
    assert_eq!(waits[0].kind, WorkflowWaitKind::Input);
    assert_eq!(waits[0].input, Some(serde_json::json!(1)));
    let canonical = representation::graph::gate_waits::page_in(&*tx, "gates", Some(1), None, 100)
        .await
        .unwrap();
    assert_eq!(waits[0].requested_at_ms, canonical.gates[0].requested_at_ms);
    assert_eq!(inspect(&*tx, "gates", 2, 100).await.1, None);
    tx.commit().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let store =
        bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path()).unwrap();
    let missing = open(store.path(), false).unwrap();
    let tx = missing.begin_transaction().await.unwrap();
    let (waits, complete) = inspect(&*tx, "gates", 1, 100).await;
    assert!(waits.is_empty());
    assert_eq!(complete, None);
    tx.commit().await.unwrap();
}
