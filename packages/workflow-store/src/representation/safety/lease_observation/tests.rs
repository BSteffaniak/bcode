use super::*;
use crate::representation as rep;
use bcode_workflow::ResourceClaim;
use switchy_database::TransactionMode;

#[tokio::test]
async fn acquisition_facts_are_atomic_idempotent_and_missing_time_fails_closed() {
    let db = rep::tests::database().await;
    rep::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "leases",
        bcode_workflow::Step::task("work", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let id =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("leases", &definition).unwrap();
    let mut run = rep::tests::run("leases", false);
    run.definition_id = id.definition_id;
    run.definition_version = id.definition_version;
    rep::admit(&*db, &run, &definition).await.unwrap();
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    rep::attempts::claim_authority(&*db, "leases", &owner)
        .await
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: "leases".into(),
        node_id: "work".into(),
        activation_id: rep::graph::activation_id(&*db, "leases", "work")
            .await
            .unwrap()
            .unwrap(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::Mutating,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch = rep::attempts::prepare(&*db, &owner, &prepared)
        .await
        .unwrap();
    let claims = [ResourceClaim::read("a"), ResourceClaim::write("b")];
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    super::super::reserve(&*tx, "leases", &dispatch, &claims)
        .await
        .unwrap();
    let first = page_in(&*tx, "leases", 2).await.unwrap();
    assert!(first.complete);
    assert_eq!(first.leases.len(), 2);
    assert!(first.leases[0].acquired_at_ms > prepared.prepared_at_ms);
    super::super::reserve(&*tx, "leases", &dispatch, &claims)
        .await
        .unwrap();
    assert_eq!(
        page_in(&*tx, "leases", 2).await.unwrap().leases,
        first.leases
    );
    assert!(!page_in(&*tx, "leases", 1).await.unwrap().complete);
    tx.rollback().await.unwrap();
    let tx = db.begin_transaction().await.unwrap();
    assert!(page_in(&*tx, "leases", 2).await.unwrap().leases.is_empty());
    tx.commit().await.unwrap();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    super::super::reserve(&*tx, "leases", &dispatch, &claims)
        .await
        .unwrap();
    tx.delete(FACTS)
        .where_eq("resource_key", "a")
        .execute(&*tx)
        .await
        .unwrap();
    assert!(page_in(&*tx, "leases", 2).await.is_err());
    super::super::release(&*tx, "leases", &dispatch)
        .await
        .unwrap();
    assert!(page_in(&*tx, "leases", 2).await.unwrap().leases.is_empty());
    tx.rollback().await.unwrap();
}
