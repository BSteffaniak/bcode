use super::*;
use bcode_workflow::WorkflowSemanticHistoryCoverage as Coverage;
async fn fixture() -> Box<dyn Database> {
    let db = super::super::super::tests::database().await;
    super::super::super::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "lifecycle",
        bcode_workflow::Step::task("worker", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let run = super::super::super::tests::run("lifecycle", false);
    super::super::super::admit(&*db, &run, &definition)
        .await
        .unwrap();
    super::super::super::admit(&*db, &run, &definition)
        .await
        .unwrap();
    db
}
async fn change(db: &dyn Database, status: &str, time: u64) {
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    let history = begin(&*tx, "lifecycle").await.unwrap();
    tx.update(super::super::super::RUNS)
        .value("status", status)
        .value("updated_at_ms", i64::try_from(time).unwrap())
        .where_eq("run_id", "lifecycle")
        .execute(&*tx)
        .await
        .unwrap();
    finish(&*tx, history, status, time).await.unwrap();
    tx.commit().await.unwrap();
}
#[tokio::test]
async fn admission_duplicate_status_and_independent_attempt_scope() {
    let db = fixture().await;
    let admission = page(&*db, "lifecycle", None, 100).await.unwrap();
    assert_eq!(admission.events.len(), 1);
    assert_eq!(admission.cursor.coverage, Coverage::CompleteFromAdmission);
    change(&*db, "failed", 2).await;
    change(&*db, "failed", 3).await;
    assert_eq!(
        page(&*db, "lifecycle", None, 100)
            .await
            .unwrap()
            .events
            .len(),
        2
    );
    // Accounting-only changes do not create a lifecycle obligation.
    db.update(super::super::super::RUNS)
        .value("consumed", 1_i64)
        .where_eq("run_id", "lifecycle")
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(
        page(&*db, "lifecycle", None, 100)
            .await
            .unwrap()
            .cursor
            .coverage,
        Coverage::CompleteFromAdmission
    );
    assert_eq!(
        super::super::page(&*db, "lifecycle", None, 100)
            .await
            .unwrap()
            .cursor
            .coverage,
        Coverage::CompleteFromAdmission
    );
}
#[tokio::test]
async fn old_status_write_never_heals_and_rollback_is_atomic() {
    let db = fixture().await;
    db.update(super::super::super::RUNS)
        .value("status", "failed")
        .where_eq("run_id", "lifecycle")
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(
        page(&*db, "lifecycle", None, 100)
            .await
            .unwrap()
            .cursor
            .coverage,
        Coverage::HistoricalGap
    );
    change(&*db, "paused", 2).await;
    assert_eq!(
        page(&*db, "lifecycle", None, 100)
            .await
            .unwrap()
            .cursor
            .coverage,
        Coverage::HistoricalGap
    );
    let before = page(&*db, "lifecycle", None, 100).await.unwrap();
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    let history = begin(&*tx, "lifecycle").await.unwrap();
    tx.update(super::super::super::RUNS)
        .value("status", "completed")
        .value("updated_at_ms", 3_i64)
        .where_eq("run_id", "lifecycle")
        .execute(&*tx)
        .await
        .unwrap();
    finish(&*tx, history, "completed", 3).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(page(&*db, "lifecycle", None, 100).await.unwrap(), before);
}
#[tokio::test]
async fn source_deletion_and_checkpoint_rewrite_cannot_hide_gaps() {
    let db = fixture().await;
    assert!(
        db.update(CHECKPOINTS)
            .value("last_verified", 99_i64)
            .where_eq("run_id", "lifecycle")
            .execute(&*db)
            .await
            .is_err()
    );
    assert!(
        db.delete(CHECKPOINTS)
            .where_eq("run_id", "lifecycle")
            .execute(&*db)
            .await
            .is_err()
    );
    db.delete(super::super::super::RUNS)
        .where_eq("run_id", "lifecycle")
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(
        page(&*db, "lifecycle", None, 100)
            .await
            .unwrap()
            .cursor
            .coverage,
        Coverage::HistoricalGap
    );
}

#[tokio::test]
async fn malformed_status_and_missing_index_fail_closed_without_repair() {
    let db = fixture().await;
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    let history = begin(&*tx, "lifecycle").await.unwrap();
    tx.update(super::super::super::RUNS)
        .value("status", "future-status")
        .value("updated_at_ms", 2_i64)
        .where_eq("run_id", "lifecycle")
        .execute(&*tx)
        .await
        .unwrap();
    assert!(finish(&*tx, history, "future-status", 2).await.is_err());
    tx.rollback().await.unwrap();
    assert_eq!(
        page(&*db, "lifecycle", None, 100)
            .await
            .unwrap()
            .events
            .len(),
        1
    );
    switchy_database::schema::drop_index("idx_lifecycle_history_event_run_sequence_v1", EVENTS)
        .execute(&*db)
        .await
        .unwrap();
    assert!(page(&*db, "lifecycle", None, 100).await.is_err());
}

#[tokio::test]
async fn more_than_one_hundred_transitions_have_pinned_bounded_pages() {
    let db = fixture().await;
    for time in 2..=105 {
        change(&*db, if time % 2 == 0 { "failed" } else { "paused" }, time).await;
    }
    let first = page(&*db, "lifecycle", None, 100).await.unwrap();
    assert_eq!(first.events.len(), 100);
    let cursor = first.next_cursor.unwrap();
    change(&*db, "failed", 106).await;
    let second = page(&*db, "lifecycle", Some(&cursor), 100).await.unwrap();
    assert_eq!(second.events.len(), 5);
    assert_eq!(second.cursor.through_sequence, 105);
    assert!(second.next_cursor.is_none());
    assert_eq!(second.cursor.coverage, Coverage::CompleteFromAdmission);
}
