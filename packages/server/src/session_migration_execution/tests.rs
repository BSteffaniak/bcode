use super::*;
use bcode_session::artifact_storage::ArtifactMaintenanceCancellation;

#[tokio::test]
async fn released_historical_upgrade_reports_actual_execution_failure() {
    let (root, state, id, _, _) =
        Box::pin(crate::storage_maintenance::historical_tests::historical_fixture()).await;
    let operations = bcode_session_migration::SessionMigrationOperations::default();
    let cancellation =
        crate::storage_maintenance::operation_cancellation(&state).expect("daemon health");
    let admission = start_session_storage_maintenance(
        id,
        root.path().to_path_buf(),
        2,
        &operations,
        MetricsRegistry::disabled(),
        cancellation,
    )
    .await;
    let mut updates = admission.operation.subscribe();
    updates
        .wait_for(|snapshot| snapshot.outcome.is_some())
        .await
        .expect("settled");
    assert!(
        matches!(
            admission.operation.snapshot().outcome,
            Some(bcode_session_models::SessionOpenTerminalOutcome::Ready)
        ),
        "actual migration outcome: {:?}",
        admission.operation.snapshot()
    );
}

#[tokio::test]
async fn busy_reader_failure_allows_immediate_fresh_admission_after_release() {
    let root = tempfile::tempdir().expect("root");
    let id = SessionId::new();
    let db = db::SessionDb::initialize_turso_in_root(id, root.path())
        .await
        .expect("initialize");
    db.database().close().await.expect("close fixture");
    let registry =
        bcode_session::storage_admission::StorageAdmissionRegistry::open_session(root.path(), id)
            .expect("registry");
    let reader = registry.admit_read(SessionId::new()).expect("reader");
    let operations = bcode_session_migration::SessionMigrationOperations::default();
    let first = start_session_storage_maintenance(
        id,
        root.path().to_path_buf(),
        bcode_session_migration::CURRENT_WRITER_EPOCH,
        &operations,
        MetricsRegistry::disabled(),
        ArtifactMaintenanceCancellation::default(),
    )
    .await;
    let mut updates = first.operation.subscribe();
    updates
        .wait_for(|snapshot| snapshot.outcome.is_some())
        .await
        .expect("failed admission settled");
    assert!(!matches!(
        first.operation.snapshot().outcome,
        Some(bcode_session_models::SessionOpenTerminalOutcome::Ready)
    ));
    reader.complete().expect("reader released");
    let retry = start_session_storage_maintenance(
        id,
        root.path().to_path_buf(),
        bcode_session_migration::CURRENT_WRITER_EPOCH,
        &operations,
        MetricsRegistry::disabled(),
        ArtifactMaintenanceCancellation::default(),
    )
    .await;
    assert!(
        retry.admitted,
        "busy admission must not poison foreground registry"
    );
    let mut updates = retry.operation.subscribe();
    updates
        .wait_for(|snapshot| snapshot.outcome.is_some())
        .await
        .expect("retry settled");
    assert!(
        matches!(
            retry.operation.snapshot().outcome,
            Some(bcode_session_models::SessionOpenTerminalOutcome::Ready)
        ),
        "{:?}",
        retry.operation.snapshot()
    );
}

#[tokio::test]
async fn missing_storage_is_classified_without_initialization() {
    let root = tempfile::tempdir().expect("root");
    let id = SessionId::new();
    assert_eq!(
        classify_session_storage_for_maintenance(id, root.path()).await,
        MaintenanceStorageClassification::Unsupported {
            reason: MaintenanceStorageBlock::Missing
        }
    );
    assert!(!db::session_db_path(root.path(), id).exists());
}

#[tokio::test]
async fn maintenance_closes_current_storage_without_runtime_handoff() {
    let root = tempfile::tempdir().expect("root");
    let id = SessionId::new();
    let database = db::SessionDb::initialize_turso_in_root(id, root.path())
        .await
        .expect("initialize");
    database.database().close().await.expect("close fixture");
    assert_eq!(
        classify_session_storage_for_maintenance(id, root.path()).await,
        MaintenanceStorageClassification::Current
    );
    let operations = bcode_session_migration::SessionMigrationOperations::default();
    let admission = start_session_storage_maintenance(
        id,
        root.path().to_path_buf(),
        bcode_session_migration::CURRENT_WRITER_EPOCH,
        &operations,
        MetricsRegistry::disabled(),
        bcode_session::artifact_storage::ArtifactMaintenanceCancellation::default(),
    )
    .await;
    assert!(admission.admitted);
    let mut updates = admission.operation.subscribe();
    updates
        .wait_for(|snapshot| snapshot.outcome.is_some())
        .await
        .expect("completion");
    assert!(
        matches!(
            admission.operation.snapshot().outcome,
            Some(bcode_session_models::SessionOpenTerminalOutcome::Ready)
        ),
        "maintenance failed: {:?}",
        admission.operation.snapshot()
    );
    let guard = lease::acquire_session_maintenance_guard(root.path(), id)
        .expect("exclusive ownership released");
    let write = lease::acquire_maintenance_session_write_lock(&guard, root.path(), id)
        .expect("write ownership released");
    let reopened = db::SessionDb::open_existing_turso_in_root(id, root.path())
        .await
        .expect("no retained connection lock");
    reopened.database().close().await.expect("close probe");
    drop(write);
    drop(guard);
    assert!(
        bcode_session_migration::retained_migration_backups(root.path(), id, 1024)
            .expect("backup inventory")
            .backups
            .is_empty()
    );
}

#[tokio::test]
async fn precancelled_maintenance_does_not_initialize_or_take_ownership() {
    let root = tempfile::tempdir().expect("root");
    let id = SessionId::new();
    let operations = bcode_session_migration::SessionMigrationOperations::default();
    let control = MaintenanceMigrationControl::default();
    control.cancel();
    let task_control = control.clone();
    let path = root.path().to_path_buf();
    let operation = operations
        .start_or_join(
            super::super::migrating_session_open_snapshot(id, 1),
            move |operation| async move {
                let reporter = MigrationExecutionProgress::new(operation);
                let error = migrate_session_storage_for_maintenance(
                    id,
                    &path,
                    &reporter,
                    &MetricsRegistry::disabled(),
                    &task_control,
                )
                .await
                .expect_err("cancelled");
                super::super::session_migration_failure_outcome(&error)
            },
        )
        .await;
    let mut updates = operation.subscribe();
    updates
        .wait_for(|snapshot| snapshot.outcome.is_some())
        .await
        .expect("cancellation acknowledged");
    assert!(matches!(
        operation.snapshot().outcome,
        Some(bcode_session_models::SessionOpenTerminalOutcome::Failed { .. })
    ));
    assert!(!db::session_db_path(root.path(), id).exists());
    assert!(!lease::session_maintenance_is_active(root.path(), id).expect("no ownership"));
}
