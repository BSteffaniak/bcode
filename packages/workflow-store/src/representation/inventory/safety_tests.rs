use super::identity_page as page;
use super::*;

#[tokio::test]
async fn rollback_preserves_boundary_and_old_run_reassignment_invalidates_both() {
    let db = super::tests::fixture().await;
    super::tests::output(&*db, "run", "a").await;
    let original = page(&*db, "run", Source::Outputs, None, 1)
        .await
        .unwrap()
        .cursor;
    let tx = db.begin_transaction().await.unwrap();
    super::tests::output(&*tx, "run", "rolled-back").await;
    tx.rollback().await.unwrap();
    assert!(
        page(&*db, "run", Source::Outputs, Some(&original), 1)
            .await
            .is_ok()
    );
    db.update("workflow_outputs_v2")
        .value("run_id", "other")
        .where_eq("output_id", "a")
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        page(&*db, "run", Source::Outputs, Some(&original), 1).await,
        Err(Error::StalePage)
    ));
    assert!(generation(&*db, "other").await.unwrap() > 0);
}

#[tokio::test]
async fn initial_unmapped_candidate_cannot_heal_after_delete() {
    let db = crate::representation::tests::database().await;
    crate::representation::install(&*db).await.unwrap();
    db.insert("workflow_execution_graph_candidates_v2")
        .value("identity", "unknown")
        .value("request_json", "{}")
        .value("link_json", "{}")
        .value("created_at_ms", 1_i64)
        .execute(&*db)
        .await
        .unwrap();
    install(&*db).await.unwrap();
    db.delete("workflow_execution_graph_candidates_v2")
        .where_eq("identity", "unknown")
        .execute(&*db)
        .await
        .unwrap();
    assert!(generation(&*db, "run").await.is_err());
    install(&*db).await.unwrap();
    assert!(generation(&*db, "run").await.is_err());
}

#[tokio::test]
async fn absent_observer_and_unknown_contract_fail_closed() {
    let db = super::tests::fixture().await;
    // Replacing a source table removes its observers; reads must not reinstall.
    switchy_database::schema::drop_table("workflow_outputs_v2")
        .execute(&*db)
        .await
        .unwrap();
    assert!(page(&*db, "run", Source::Outputs, None, 1).await.is_err());
    assert!(install(&*db).await.is_err());
    let db = super::tests::fixture().await;
    db.insert(CONTRACT)
        .value("version", 99_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(page(&*db, "run", Source::Outputs, None, 1).await.is_err());
}
