//! Initial access tracking holds registered-reader admission during execution.
use super::*;
use bcode_session::artifact_storage::initialize_maintenance_access_cancellable;

#[tokio::test]
async fn initialization_refuses_registered_readers_and_cancelled_work_without_creating_age() {
    let (root, state, id, _, _) = tests::fixture(1).await;
    let access = root.path().join(id.to_string()).join("storage-access.bin");
    std::fs::remove_file(&access).expect("unknown age fixture");
    let registry =
        bcode_session::storage_admission::StorageAdmissionRegistry::open_session(root.path(), id)
            .expect("registry");
    let reader_id = SessionId::new();
    let read = registry.admit_read(reader_id).expect("live read");
    let cancellation = operation_cancellation(&state).expect("daemon acknowledgement");
    assert!(
        initialize_maintenance_access_cancellable(
            root.path(),
            id,
            crate::current_time_ms(),
            cancellation
        )
        .await
        .is_err()
    );
    assert!(!access.exists());
    read.complete().expect("clean read");
    registry.retire(reader_id).expect("retired reader");
    let cancellation = operation_cancellation(&state).expect("daemon acknowledgement");
    cancellation.cancel();
    assert!(
        initialize_maintenance_access_cancellable(
            root.path(),
            id,
            crate::current_time_ms(),
            cancellation
        )
        .await
        .is_err()
    );
    assert!(!access.exists());
    initialize_maintenance_access_cancellable(
        root.path(),
        id,
        crate::current_time_ms(),
        operation_cancellation(&state).expect("ack"),
    )
    .await
    .expect("idle initialization");
    let initial = std::fs::read(&access).expect("initialized");
    initialize_maintenance_access_cancellable(
        root.path(),
        id,
        crate::current_time_ms() + 31 * 86_400_000,
        operation_cancellation(&state).expect("ack"),
    )
    .await
    .expect("existing age unchanged");
    assert_eq!(std::fs::read(&access).expect("unchanged"), initial);
}
