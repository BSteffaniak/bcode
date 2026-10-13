use super::identity_page as page;
use super::*;
use crate::representation::tests::database;
pub(super) async fn fixture() -> Box<dyn Database> {
    let db = database().await;
    crate::representation::install(&*db).await.unwrap();
    db.insert("workflow_graphs_v2")
        .value("run_id", "run")
        .value("revision", 1_i64)
        .value("definition_json", "{}")
        .value("edges_json", "[]")
        .execute(&*db)
        .await
        .unwrap();
    install(&*db).await.unwrap();
    db
}
pub(super) async fn output(db: &dyn Database, run: &str, id: &str) {
    db.insert("workflow_outputs_v2")
        .value("output_id", id)
        .value("run_id", run)
        .value("activation_id", id)
        .value("output_json", "{}")
        .execute(db)
        .await
        .unwrap();
}
#[tokio::test]
async fn bounded_outputs_and_mutations_reject_original_boundary() {
    let db = fixture().await;
    for n in 0..105 {
        output(&*db, "run", &format!("out-{n:03}")).await;
    }
    let first = page(&*db, "run", Source::Outputs, None, 100).await.unwrap();
    assert_eq!(first.identities.len(), 100);
    let cursor = first.next.unwrap();
    let second = page(&*db, "run", Source::Outputs, Some(&cursor), 100)
        .await
        .unwrap();
    assert_eq!(second.identities.len(), 5);
    assert!(second.next.is_none());
    assert!(
        page(&*db, "other", Source::Outputs, Some(&cursor), 100)
            .await
            .is_err()
    );
    assert!(
        page(&*db, "run", Source::Children, Some(&cursor), 100)
            .await
            .is_err()
    );
    output(&*db, "run", "before").await;
    assert!(matches!(
        page(&*db, "run", Source::Outputs, Some(&cursor), 100).await,
        Err(Error::StalePage)
    ));
    let current = page(&*db, "run", Source::Outputs, None, 1)
        .await
        .unwrap()
        .next
        .unwrap();
    db.delete("workflow_outputs_v2")
        .where_eq("output_id", "before")
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        page(&*db, "run", Source::Outputs, Some(&current), 100).await,
        Err(Error::StalePage)
    ));
    let current = page(&*db, "run", Source::Outputs, None, 1)
        .await
        .unwrap()
        .next
        .unwrap();
    db.update("workflow_outputs_v2")
        .value("output_id", "aaa")
        .where_eq("output_id", "out-000")
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        page(&*db, "run", Source::Outputs, Some(&current), 100).await,
        Err(Error::StalePage)
    ));
}
#[tokio::test]
async fn unmapped_old_writer_is_permanent_gap_and_install_does_not_heal() {
    let db = fixture().await;
    db.insert("workflow_execution_sessions_v2")
        .value("dispatch_identity", "unknown")
        .value("link_json", "{}")
        .execute(&*db)
        .await
        .unwrap();
    assert!(page(&*db, "run", Source::Outputs, None, 1).await.is_err());
    db.delete("workflow_execution_sessions_v2")
        .where_eq("dispatch_identity", "unknown")
        .execute(&*db)
        .await
        .unwrap();
    install(&*db).await.unwrap();
    assert!(page(&*db, "run", Source::Outputs, None, 100).await.is_err());
}
#[tokio::test]
async fn missing_index_rejected_without_repair() {
    let db = fixture().await;
    // Typed schema operations only; no raw database access.
    switchy_database::schema::drop_index("idx_inventory_outputs_run_key", "workflow_outputs_v2")
        .execute(&*db)
        .await
        .unwrap();
    assert!(page(&*db, "run", Source::Outputs, None, 1).await.is_err());
    assert!(install(&*db).await.is_err());
}
#[tokio::test]
async fn receipt_updates_and_session_links_pin_examined_attempts() {
    let db = fixture().await;
    for id in ["a", "b", "c"] {
        db.insert("workflow_attempts_v2")
            .value("dispatch_identity", id)
            .value("run_id", "run")
            .value("prepared_json", "{}")
            .value("node_id", id)
            .value("activation_id", id)
            .value("attempt_number", 1_i64)
            .execute(&*db)
            .await
            .unwrap();
    }
    let first = page(&*db, "run", Source::SessionLinks, None, 1)
        .await
        .unwrap();
    assert!(first.identities.is_empty());
    let cursor = first.next.unwrap();
    assert_eq!(cursor.after.as_deref(), Some("a"));
    db.insert("workflow_execution_sessions_v2")
        .value("dispatch_identity", "a")
        .value("link_json", "{}")
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        page(&*db, "run", Source::SessionLinks, Some(&cursor), 1).await,
        Err(Error::StalePage)
    ));
    let current = page(&*db, "run", Source::SessionLinks, None, 1)
        .await
        .unwrap()
        .next
        .unwrap();
    db.update("workflow_attempts_v2")
        .value("receipt_json", "{}")
        .where_eq("dispatch_identity", "a")
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        page(&*db, "run", Source::SessionLinks, Some(&current), 1).await,
        Err(Error::StalePage)
    ));
}
