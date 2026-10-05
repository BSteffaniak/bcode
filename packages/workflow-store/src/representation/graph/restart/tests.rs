use super::super::super::{attempts, tests::run};
use super::super::tests::success;
use super::*;

#[tokio::test]
async fn large_restart_reopens_and_admits_only_after_all_retirements() {
    let temp = tempfile::tempdir().unwrap();
    let _legacy = crate::WorkflowStore::create_coexistence_baseline(temp.path()).unwrap();
    let path = crate::workflow_database_path(temp.path());
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    super::super::super::install(&*db).await.unwrap();
    let definition = large_definition();
    let mut request = run("graph", false);
    request.input = Some(serde_json::json!({"done":false,"iteration":1}));
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("large-repeat", &definition)
            .unwrap();
    request.definition_id = exact.definition_id;
    request.definition_version = exact.definition_version;
    super::super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    attempts::claim_authority(&*db, "graph", &owner)
        .await
        .unwrap();
    let original = super::super::activation_id(&*db, "graph", "n000")
        .await
        .unwrap()
        .unwrap();
    for index in 0..130 {
        let node = format!("n{index:03}");
        success(&*db, &owner, &node).await;
        super::super::settle_output(
            &*db,
            &owner,
            "graph",
            &node,
            1,
            request.input.as_ref().unwrap(),
        )
        .await
        .unwrap();
    }
    let repeat = super::super::activation_id(&*db, "graph", "repeat")
        .await
        .unwrap()
        .unwrap();
    super::super::control::settle(&*db, &owner, "graph", "repeat", &repeat, 1)
        .await
        .unwrap();
    assert!(pending(&*db, "graph").await.unwrap());
    assert!(
        super::super::ready(&*db, "graph", 1, None, 100)
            .await
            .unwrap()
            .is_empty()
    );
    for _ in 0..5 {
        assert!(
            super::super::frontier::advance(&*db, &owner, "graph", 7)
                .await
                .unwrap()
                <= 7
        );
    }
    drop(db);
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    let mut count = 0;
    while pending(&*db, "graph").await.unwrap() {
        assert!(
            super::super::ready(&*db, "graph", 1, None, 100)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            super::super::frontier::advance(&*db, &owner, "graph", 7)
                .await
                .unwrap()
                <= 7
        );
        count += 1;
        assert!(count < 1000);
    }
    let ready = super::super::ready(&*db, "graph", 1, None, 100)
        .await
        .unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].node_id, "n000");
    assert_eq!(ready[0].generation, 2);
    assert_ne!(ready[0].activation_id, original);
    assert_eq!(ready[0].input["iteration"], 2);
    assert_eq!(
        super::super::frontier::advance(&*db, &owner, "graph", 7)
            .await
            .unwrap(),
        0
    );
}

fn large_definition() -> bcode_workflow::WorkflowDefinition {
    let step = |id: &str| bcode_workflow::Step::task(id, |v: Value, _| async move { Ok(v) });
    let mut body = step("n000");
    for index in 1..130 {
        body = body.then(step(&format!("n{index:03}")));
    }
    bcode_workflow::WorkflowBuilder::new(
        "large-repeat",
        body.repeat_while(
            "repeat",
            bcode_workflow::field::<Value>("done").eq(false),
            2,
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone()
}

#[tokio::test]
async fn restart_validation_blocks_unsettled_body_before_any_retirement() {
    let db = super::super::super::tests::database().await;
    super::super::super::install(&*db).await.unwrap();
    let definition = large_definition();
    let mut request = run("graph", false);
    request.input = Some(serde_json::json!({"done":false,"iteration":1}));
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("blocked-restart", &definition)
            .unwrap();
    request.definition_id = exact.definition_id;
    request.definition_version = exact.definition_version;
    super::super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    // Simulate a ready repeat while another body activation still has unresolved work. No effects
    // are asserted absent; discovery/validation may progress but retirement must never start.
    let tx = db
        .begin_transaction_with_mode(super::super::TransactionMode::Immediate)
        .await
        .unwrap();
    super::super::insert_activation(&*tx, "graph", "repeat", request.input.as_ref().unwrap())
        .await
        .unwrap();
    begin(&*tx, &request, 1, "repeat", request.input.as_ref().unwrap())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let original = super::super::activation_id(&*db, "graph", "n000")
        .await
        .unwrap();
    let mut blocked = false;
    for _ in 0..100 {
        let tx = db
            .begin_transaction_with_mode(super::super::TransactionMode::Immediate)
            .await
            .unwrap();
        match drain_in(&*tx, "graph", 7).await {
            Ok(_) => tx.commit().await.unwrap(),
            Err(Error::UnsupportedLifecycle) => {
                blocked = true;
                break;
            }
            Err(error) => panic!("{error:?}"),
        }
    }
    assert!(blocked);
    assert!(pending(&*db, "graph").await.unwrap());
    assert_eq!(
        super::super::activation_id(&*db, "graph", "n000")
            .await
            .unwrap(),
        original
    );
    assert!(
        db.select(VISITS)
            .where_eq("phase", "done")
            .limit(1)
            .execute(&*db)
            .await
            .unwrap()
            .is_empty()
    );
}
