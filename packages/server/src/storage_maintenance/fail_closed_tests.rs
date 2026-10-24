//! Fail-closed behavior before historical mutation.
use super::historical_tests::historical_fixture;
use super::*;

#[tokio::test]
async fn registered_reader_prevents_historical_upgrade_and_backup_creation() {
    let (root, state, id, artifacts, _) = Box::pin(historical_fixture()).await;
    let registry =
        bcode_session::storage_admission::StorageAdmissionRegistry::open_session(root.path(), id)
            .expect("registry");
    let reader = registry
        .admit_read(SessionId::new())
        .expect("active reader");
    let now = crate::current_time_ms() + 31 * 86_400_000;
    assert_eq!(
        maintain_session_at(&state, root.path(), id, None, now)
            .await
            .expect_err("reader blocks upgrade"),
        "tracking_unavailable"
    );
    assert!(artifacts.join("recording-000").is_file());
    assert!(!root.path().join("session-migration-backups").exists());
    drop(reader);
}

#[tokio::test]
async fn corrupt_access_and_future_writer_preserve_historical_storage_without_backups() {
    let (root, state, id, artifacts, _) = Box::pin(historical_fixture()).await;
    let access = root.path().join(id.to_string()).join("storage-access.bin");
    let age = std::fs::read(&access).expect("age fixture");
    std::fs::write(&access, b"damaged").expect("damage fixture");
    let now = crate::current_time_ms() + 31 * 86_400_000;
    assert!(
        maintain_session_at(&state, root.path(), id, None, now)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(&access).expect("preserved damage"),
        b"damaged"
    );
    assert!(artifacts.join("recording-000").is_file());
    assert!(!root.path().join("session-migration-backups").exists());
    std::fs::write(&access, age).expect("restore fixture age");
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(id, root.path())
        .await
        .expect("fixture db");
    db.database()
        .update("session_storage_contract")
        .value("writer_epoch", i64::MAX)
        .execute(db.database())
        .await
        .expect("future fixture");
    db.database().close().await.expect("close fixture");
    drop(db);
    assert_eq!(
        maintain_session_at(&state, root.path(), id, None, now)
            .await
            .expect_err("future rejected"),
        "future_writer"
    );
    assert!(artifacts.join("recording-000").is_file());
    assert!(!root.path().join("session-migration-backups").exists());
}
