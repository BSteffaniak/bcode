//! Cross-process coverage for actor read-connection lifetimes.

use super::*;
use crate::SessionManager;

#[test]
#[ignore = "subprocess helper for read connection lifetime tests"]
fn read_connection_open_helper() {
    let root = std::env::var_os("BCODE_READ_CONNECTION_TEST_ROOT").expect("test root");
    let session_id = std::env::var("BCODE_READ_CONNECTION_TEST_SESSION")
        .expect("session id")
        .parse()
        .expect("valid session id");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(crate::db_connection::without_open_retries(async {
            let db =
                SessionDb::open_existing_turso_in_root(session_id, std::path::Path::new(&root))
                    .await
                    .expect("another process must open immediately after the read reply");
            db.validate_read_readiness().await.unwrap();
        }));
}

fn assert_other_process_can_open(root: &std::path::Path, session_id: SessionId) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "actor::read_connection_tests::read_connection_open_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("BCODE_READ_CONNECTION_TEST_ROOT", root)
        .env("BCODE_READ_CONNECTION_TEST_SESSION", session_id.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "subprocess failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn released_actor_reads_do_not_retain_database_locks() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::persistent(root.path()).unwrap();
    let session = manager
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let client = ClientId::new();
    manager
        .attach_session_recent(session.id, client, 8)
        .await
        .unwrap();
    manager.detach_session(session.id, client).await.unwrap();
    assert!(!manager.session_is_owned(session.id).await);
    assert_other_process_can_open(root.path(), session.id);

    let handle = manager.session_handle(session.id).await.unwrap();
    for _ in 0..3 {
        assert_eq!(
            manager.session_health(session.id).await,
            crate::SessionHealth::Ready
        );
        assert!(!manager.session_is_owned(session.id).await);
        assert_other_process_can_open(root.path(), session.id);
        manager
            .session_history_page(
                session.id,
                SessionHistoryQuery {
                    cursor: None,
                    limit: 8,
                    direction: SessionHistoryDirection::Backward,
                },
            )
            .await
            .unwrap();
        assert_other_process_can_open(root.path(), session.id);
    }
    // Exercise separate command families; every reply must end its unowned read lifetime.
    handle.observed_summary().await.unwrap();
    assert_other_process_can_open(root.path(), session.id);
    handle.observed_child_page(None, 8).await.unwrap();
    assert_other_process_can_open(root.path(), session.id);
    handle.composer_draft().await.unwrap();
    assert_other_process_can_open(root.path(), session.id);
    handle.input_history_page(None, 8).await.unwrap();
    assert_other_process_can_open(root.path(), session.id);
    handle.current_context_occupancy().await.unwrap();
    assert_other_process_can_open(root.path(), session.id);
    handle.submission_queue_status().await.unwrap();
    assert_other_process_can_open(root.path(), session.id);
    handle.active_runtime_work().await.unwrap();
    assert_other_process_can_open(root.path(), session.id);
    handle.active_tool_runs().await.unwrap();
    assert_other_process_can_open(root.path(), session.id);
    handle.milestone_evaluation_checkpoint().await.unwrap();
    assert_other_process_can_open(root.path(), session.id);
    handle
        .durable_interaction_source("missing".into(), "missing".into())
        .await
        .unwrap_err();
    assert_other_process_can_open(root.path(), session.id);
    assert!(!manager.session_is_owned(session.id).await);
    manager.shutdown_catalog_updates().await;
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn summary_actor_and_abandoned_read_reply_do_not_pin_database() {
    let root = tempfile::tempdir().unwrap();
    let original = SessionManager::persistent(root.path()).unwrap();
    let session = original
        .create_session(None, root.path().into())
        .await
        .unwrap();
    original
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    original.shutdown_catalog_updates().await;
    original
        .session_handle(session.id)
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();

    let manager = SessionManager::persistent(root.path()).unwrap();
    let handle = manager.session_handle(session.id).await.unwrap();
    assert!(!handle.snapshot().owned);
    handle.events_range(0, 0, 1).await.unwrap();
    assert_other_process_can_open(root.path(), session.id);
    let (reply, receiver) = oneshot::channel();
    drop(receiver);
    handle
        .commands
        .send(SessionCommand::Health {
            root: root.path().into(),
            reply,
        })
        .await
        .unwrap();
    // Mailbox barrier: even when delivery is abandoned, the completed read releases its handle.
    handle.summary().await.unwrap();
    assert_other_process_can_open(root.path(), session.id);
    manager
        .attach_session_recent(session.id, ClientId::new(), 8)
        .await
        .unwrap();
    assert!(manager.session_is_owned(session.id).await);
    handle.shutdown().await.unwrap();
    manager.shutdown_catalog_updates().await;
}

#[tokio::test]
async fn unowned_actor_reads_preserve_pending_wal_bytes() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::persistent(root.path()).unwrap();
    let session = manager
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let handle = manager.session_handle(session.id).await.unwrap();
    let mut event = handle.events_range(0, 0, 1).await.unwrap().remove(0);
    manager
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    manager.shutdown_catalog_updates().await;

    // Leave committed data in the WAL. Checkpointing close would change these bytes.
    let db = SessionDb::open_existing_turso_in_root(session.id, root.path())
        .await
        .unwrap();
    event.sequence = 1;
    event.kind = SessionEventKind::SystemMessage {
        text: "pending WAL".into(),
    };
    db.append_event(&event).await.unwrap();
    drop(db);
    let files = || {
        let path = crate::db::session_db_path(root.path(), session.id);
        let mut files: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("session.db")
            })
            .map(|entry| (entry.file_name(), std::fs::read(entry.path()).unwrap()))
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    };
    let before = files();
    assert!(
        before
            .iter()
            .any(|(name, bytes)| name == "session.db-wal" && !bytes.is_empty())
    );
    assert_eq!(
        manager.session_health(session.id).await,
        crate::SessionHealth::Ready
    );
    assert_eq!(handle.events_range(1, 1, 1).await.unwrap(), vec![event]);
    assert_eq!(
        files(),
        before,
        "scoped reads must not checkpoint or change sidecars"
    );
    assert_other_process_can_open(root.path(), session.id);
    handle.shutdown().await.unwrap();
}
