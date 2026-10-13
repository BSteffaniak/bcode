use super::*;
use bcode_workflow::{
    WorkflowInventoryCoverage as Coverage, WorkflowInventoryEntry as Entry,
    WorkflowInventoryQuery as Query, WorkflowInventorySource as Source,
};
#[tokio::test]
async fn public_outputs_pin_source_and_report_missing_stored_proof() {
    let db = super::tests::fixture().await;
    for n in 0..105 {
        super::tests::output(&*db, "run", &format!("out-{n:03}")).await;
    }
    let mut query = Query {
        version: 1,
        run_id: "run".into(),
        source: Source::Outputs,
        cursor: None,
        limit: 100,
    };
    let first = page(&*db, &query).await.unwrap();
    query.validate_page(&first).unwrap();
    assert_eq!(first.entries.len(), 100);
    assert!(matches!(first.coverage, Coverage::CurrentInventoryComplete));
    assert!(matches!(&first.entries[0], Entry::OutputUnavailable { .. }));
    query.cursor = first.next_cursor;
    let second = page(&*db, &query).await.unwrap();
    assert_eq!(second.entries.len(), 5);
    assert!(second.next_cursor.is_none());
    // A different source changes the global ledger but not the output source pin.
    db.update("workflow_graphs_v2")
        .value("definition_json", "{}")
        .where_eq("run_id", "run")
        .execute(&*db)
        .await
        .unwrap();
    assert!(page(&*db, &query).await.is_ok());
    super::tests::output(&*db, "run", "aaa").await;
    assert!(matches!(
        page(&*db, &query).await,
        Err(RepresentationError::InventoryStalePage)
    ));
}
#[tokio::test]
async fn public_unmapped_capture_is_unavailable_not_empty_complete() {
    let db = super::tests::fixture().await;
    db.insert("workflow_execution_sessions_v2")
        .value("dispatch_identity", "unknown")
        .value("link_json", "{}")
        .execute(&*db)
        .await
        .unwrap();
    let query = Query {
        version: 1,
        run_id: "run".into(),
        source: Source::Outputs,
        cursor: None,
        limit: 100,
    };
    let result = page(&*db, &query).await.unwrap();
    query.validate_page(&result).unwrap();
    assert!(matches!(result.coverage, Coverage::Unavailable { .. }));
    assert!(result.entries.is_empty());
    assert!(result.next_cursor.is_none());
}
