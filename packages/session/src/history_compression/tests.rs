//! Whole maintenance-path tests using current session APIs and real database files.

use super::*;
use bcode_session_models::{SessionEventKind, SessionHistoryDirection, SessionHistoryQuery};

#[tokio::test]
async fn paged_history_compression_reclaims_real_space_and_preserves_reopen_and_append() {
    let root = tempfile::tempdir().expect("root");
    let manager = crate::SessionManager::persistent(root.path()).expect("manager");
    let session = manager
        .create_session(Some("compression".into()), root.path().to_path_buf())
        .await
        .expect("create");
    let id = session.id;
    for index in 0..40 {
        manager
            .append_event(
                id,
                SessionEventKind::SystemMessage {
                    text: format!("{index}:{}", "lossless 世界 history ".repeat(8000)),
                },
            )
            .await
            .expect("append");
    }
    let expected = manager.session_history(id).await.expect("history");
    manager
        .release_session_ownership(id)
        .await
        .expect("release");
    drop(manager);
    let path = root.path().join(id.to_string()).join("session.db");
    let before = std::fs::metadata(&path).expect("before").len();
    let mut cursor = 0;
    let mut pages = 0;
    let mut saved = 0;
    let mut inspected = 0;
    loop {
        let page = compress_history_page(root.path(), id, cursor, 1)
            .await
            .expect("compress page");
        assert!(page.inspected <= 16);
        pages += 1;
        saved += page.saved_bytes;
        inspected += page.inspected;
        let Some(next) = page.next_sequence else {
            break;
        };
        assert!(next > cursor);
        cursor = next;
    }
    assert!(pages >= 4);
    assert_eq!(inspected, expected.len());
    assert!(saved > 4 * 1024 * 1024);
    let report = crate::storage_reclamation::reclaim_session_storage(root.path(), id)
        .await
        .expect("reclaim");
    assert!(report.reclaimed_bytes() > 4 * 1024 * 1024, "{report:?}");
    assert!(std::fs::metadata(&path).expect("after").len() < before);
    let reopened = crate::SessionManager::persistent(root.path()).expect("reopen");
    assert_eq!(
        reopened.session_history(id).await.expect("same history"),
        expected
    );
    let page = reopened
        .session_history_page(
            id,
            SessionHistoryQuery {
                cursor: None,
                limit: 3,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await
        .expect("bounded page");
    assert_eq!(page.events.len(), 3);
    let last = reopened
        .append_event(
            id,
            SessionEventKind::SystemMessage {
                text: "continued after reclamation".into(),
            },
        )
        .await
        .expect("continue");
    let history = reopened
        .session_history(id)
        .await
        .expect("continued history");
    assert_eq!(&history[..expected.len()], expected);
    assert_eq!(history.last(), Some(&last));
    reopened
        .release_session_ownership(id)
        .await
        .expect("release");
}

#[tokio::test]
async fn incompatible_writer_cannot_recompress_canonical_payloads() {
    let root = tempfile::tempdir().expect("root");
    let manager = crate::SessionManager::persistent(root.path()).expect("manager");
    let session = manager
        .create_session(Some("history".repeat(10000)), root.path().to_path_buf())
        .await
        .expect("create");
    let id = session.id;
    manager
        .release_session_ownership(id)
        .await
        .expect("release");
    drop(manager);
    let db = SessionDb::open_existing_turso_in_root(id, root.path())
        .await
        .expect("db");
    let logical = db.canonical_rows_page(0, 1).await.expect("before");
    db.database()
        .exec_raw("UPDATE session_storage_contract SET writer_epoch = 9 WHERE contract_id = 1")
        .await
        .expect("old writer fixture");
    db.database().close().await.expect("close");
    drop(db);
    assert!(compress_history_page(root.path(), id, 0, 1).await.is_err());
    let db = SessionDb::open_existing_turso_in_root(id, root.path())
        .await
        .expect("inspect");
    assert_eq!(db.storage_writer_epoch().await.expect("preserved epoch"), 9);
    assert_eq!(
        db.canonical_rows_page(0, 1).await.expect("unchanged")[0].payload,
        logical[0].payload
    );
}
