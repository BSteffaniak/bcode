//! The actual daemon loop accepts released historical storage without foreground open.
use super::historical_tests::historical_fixture;
use super::*;

#[tokio::test]
async fn worker_loop_upgrades_released_history_then_compresses_both_tiers() {
    let (root, mut state, id, artifacts, expected) = Box::pin(historical_fixture()).await;
    state
        .startup_config
        .session_storage
        .maintenance_interval_secs = 1;
    let access = root.path().join(id.to_string()).join("storage-access.bin");
    let before = std::fs::read(&access).expect("original age");
    let state = Arc::new(state);
    let task = tokio::spawn(run_with_clock(Arc::clone(&state), || {
        crate::current_time_ms() + 31 * 86_400_000
    }));
    let completed = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let snapshot = state.metrics.snapshot();
            if snapshot
                .counters
                .get("storage.maintenance.history_payload_bytes_saved")
                .copied()
                .unwrap_or_default()
                > 0
                && artifacts.join("recording-000").is_dir()
            {
                break Ok(());
            }
            if let Some(terminal) = state.storage_worker.terminal(id)
                && terminal.outcome != Some(bcode_session_models::SessionOpenTerminalOutcome::Ready)
            {
                break Err(format!("historical worker migration failed: {terminal:?}"));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    state.request_shutdown();
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("bounded settled shutdown")
        .expect("worker task");
    completed
        .unwrap_or_else(|error| {
            panic!(
                "worker acceptance timeout: {error}; terminal={:?}; status={:?}",
                state.storage_worker.terminal(id),
                state.storage_worker.status(id)
            )
        })
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(std::fs::read(&access).expect("unchanged age"), before);
    assert!(
        !bcode_session_migration::retained_migration_backups(root.path(), id, 256)
            .expect("backups")
            .backups
            .is_empty()
    );
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(id, root.path())
        .await
        .expect("current db");
    let page = db
        .history_page(bcode_session_models::SessionHistoryQuery {
            cursor: None,
            direction: bcode_session_models::SessionHistoryDirection::Forward,
            limit: 100,
        })
        .await
        .expect("strict current history");
    assert_eq!(page.events, expected);
    db.database().close().await.expect("closed");
    assert!(state.attached_client_sessions.lock().await.is_empty());
    assert!(state.session_runtimes.lock().await.is_empty());
    drop(state);
}
