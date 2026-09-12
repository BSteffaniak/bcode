//! Application-level access tracking integration tests.

use super::*;
use bcode_session::storage_access::{StorageAccessObservation, observe_access};

#[tokio::test]
async fn scheduling_opt_out_keeps_consumption_timestamps_current() {
    let root = tempfile::tempdir().expect("root");
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
    let session = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .expect("session");
    let state = crate::tests::test_server_state(sessions);
    let mut config = bcode_config::BcodeConfig::default();
    config.session_storage.enabled = false;
    state
        .session_configs
        .lock()
        .await
        .insert(session.id, config);
    let path = root
        .path()
        .join(session.id.to_string())
        .join("storage-access.bin");
    for kind in [
        bcode_session::storage_access::StorageAccessKind::History,
        bcode_session::storage_access::StorageAccessKind::Artifact,
        bcode_session::storage_access::StorageAccessKind::ModelContext,
    ] {
        // Initialize an old valid timestamp to prove each category refreshes it.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("tracking");
        bcode_session::storage_access::record_access(&mut file, kind, 1).expect("old timestamp");
        record_consumption(&state, session.id, kind).await;
        let StorageAccessObservation::Recorded(record) =
            observe_access(&mut file).expect("tracking")
        else {
            panic!("recorded")
        };
        assert!(record.observed_at_ms > 1);
        assert_eq!(record.generation, 2);
    }
    drop(state);
}

#[tokio::test]
async fn failed_history_read_does_not_create_session_or_tracking() {
    let root = tempfile::tempdir().expect("root");
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
    let state = crate::tests::test_server_state(sessions);
    let id = bcode_session_models::SessionId::new();
    assert!(
        history_page(
            &state,
            bcode_session_models::ClientId::new(),
            id,
            bcode_session_models::SessionHistoryQuery {
                cursor: None,
                limit: 10,
                direction: bcode_session_models::SessionHistoryDirection::Backward,
            }
        )
        .await
        .is_err()
    );
    drop(state);
    assert!(!root.path().join(id.to_string()).exists());
}

#[tokio::test]
async fn explicit_history_tracks_but_background_history_does_not() {
    let root = tempfile::tempdir().expect("root");
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
    let session = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .expect("session");
    let path = root
        .path()
        .join(session.id.to_string())
        .join("storage-access.bin");
    sessions
        .session_history_page(
            session.id,
            bcode_session_models::SessionHistoryQuery {
                cursor: None,
                limit: 10,
                direction: bcode_session_models::SessionHistoryDirection::Backward,
            },
        )
        .await
        .expect("background page");
    assert!(!path.exists());
    let state = crate::tests::test_server_state(sessions);
    let client = bcode_session_models::ClientId::new();
    let page = history_page(
        &state,
        client,
        session.id,
        bcode_session_models::SessionHistoryQuery {
            cursor: None,
            limit: 10,
            direction: bcode_session_models::SessionHistoryDirection::Backward,
        },
    )
    .await
    .expect("page");
    assert!(!page.events.is_empty());
    let mut file = std::fs::File::open(&path).expect("tracking");
    assert!(matches!(
        observe_access(&mut file).expect("record"),
        StorageAccessObservation::Recorded(_)
    ));
    drop(file);
    std::fs::write(&path, b"damaged").expect("damage");
    let repeated = history_page(
        &state,
        client,
        session.id,
        bcode_session_models::SessionHistoryQuery {
            cursor: None,
            limit: 10,
            direction: bcode_session_models::SessionHistoryDirection::Backward,
        },
    )
    .await
    .expect("healthy read");
    drop(state);
    assert_eq!(repeated.events.len(), page.events.len());
    assert_eq!(std::fs::read(&path).expect("preserved"), b"damaged");
}
