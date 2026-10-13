use super::*;

#[tokio::test]
async fn caller_snapshot_wait_reader_does_not_own_transaction_lifecycle() {
    let (db, _) = fixture().await;
    let tx = db.begin_transaction().await.unwrap();
    let first = page_in(&*tx, "waits", Some(1), None, 1).await.unwrap();
    assert_eq!(first.gates.len(), 1);
    assert_eq!(first.gates[0].kind, GateKind::Input);
    assert_eq!(first.gates[0].input, serde_json::json!(1));
    assert!(page_in(&*tx, "waits", Some(2), None, 1).await.is_err());
    let second = page_in(&*tx, "waits", Some(1), None, 1).await.unwrap();
    assert_eq!(first, second);
    tx.rollback().await.unwrap();
    assert_eq!(page(&*db, "waits", 1).await.unwrap(), first);
}
