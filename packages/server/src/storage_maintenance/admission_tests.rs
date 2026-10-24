//! Foreground and shared-operation admission behavior.
use super::historical_tests::historical_fixture;
use super::*;

#[tokio::test]
async fn pending_foreground_attach_defers_without_upgrading_or_initializing_age() {
    let (root, state, id, artifacts, _) = Box::pin(historical_fixture()).await;
    state.active_session_namespaces.lock().await.insert(
        id,
        crate::ActiveSessionNamespace {
            namespace: "foreground-fixture".into(),
            pending_attaches: 1,
        },
    );
    let access = root.path().join(id.to_string()).join("storage-access.bin");
    std::fs::remove_file(&access).expect("unknown fixture age");
    assert_eq!(
        maintain_session_at(&state, root.path(), id, None, crate::current_time_ms())
            .await
            .expect("foreground skipped"),
        None
    );
    assert!(!access.exists());
    assert!(artifacts.join("recording-000").is_file());
    assert!(!root.path().join("session-migration-backups").exists());
    assert_eq!(
        state.storage_worker.status(id).reason.as_deref(),
        Some("foreground_pending")
    );
}

#[tokio::test]
async fn shutdown_waits_for_joined_operation_without_cancelling_its_work() {
    let (root, state, id, artifacts, _) = Box::pin(historical_fixture()).await;
    let state = Arc::new(state);
    let (finish, done) = tokio::sync::oneshot::channel();
    let operation = state
        .session_migrations
        .operations()
        .start_or_join(
            crate::migrating_session_open_snapshot(id, 2),
            move |_| async move {
                done.await.expect("joined work must not be cancelled");
                bcode_session_models::SessionOpenTerminalOutcome::Ready
            },
        )
        .await;
    let work_state = Arc::clone(&state);
    let path = root.path().to_path_buf();
    let work = tokio::spawn(async move {
        upgrade::prepare(&work_state, &path, id, crate::current_time_ms(), 60).await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while state
            .metrics
            .snapshot()
            .counters
            .get("storage.maintenance.reason.upgrade_joined")
            .copied()
            .unwrap_or_default()
            == 0
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("joined shared registry");
    state.request_shutdown();
    drop(state);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !work.is_finished(),
        "join remains owned through actual completion"
    );
    finish.send(()).expect("joined runner still present");
    assert_eq!(
        work.await.expect("worker result").expect_err("shutdown"),
        "cancelled"
    );
    assert_eq!(
        operation.snapshot().outcome,
        Some(bcode_session_models::SessionOpenTerminalOutcome::Ready)
    );
    assert!(artifacts.join("recording-000").is_file());
}
