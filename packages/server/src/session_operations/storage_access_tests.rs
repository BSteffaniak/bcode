//! Application-level access tracking integration tests.

use super::*;
use bcode_session::storage_access::{StorageAccessObservation, observe_access};

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
