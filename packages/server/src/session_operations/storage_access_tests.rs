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
async fn projection_history_pages_cross_application_boundary_without_writing_on_failure() {
    use bcode_session_models::{
        ProjectionWindowAnchor, ProjectionWindowDirection, ProjectionWindowLimits,
        ProjectionWindowRequest, ProjectionWindowTarget, SessionProjectionKind,
    };

    let root = tempfile::tempdir().expect("root");
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
    let session = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .expect("session");
    let client = bcode_session_models::ClientId::new();
    for index in 0..5 {
        sessions
            .append_user_message(session.id, client, format!("message {index}"))
            .await
            .expect("message");
    }
    let state = crate::tests::test_server_state(sessions);
    let request = |anchor, direction| ProjectionWindowRequest {
        projection: SessionProjectionKind::Transcript,
        anchor,
        direction,
        target: ProjectionWindowTarget {
            min_items: Some(1),
            width_columns: Some(80),
            ..ProjectionWindowTarget::default()
        },
        limits: ProjectionWindowLimits {
            max_items: 1,
            max_events_scanned: 5,
            max_bytes: 4096,
        },
    };
    let latest = projection_history_page(
        &state,
        client,
        session.id,
        request(
            ProjectionWindowAnchor::Latest,
            ProjectionWindowDirection::Backward,
        ),
    )
    .await
    .expect("latest page");
    assert_eq!(latest.events.len(), 1);
    assert!(latest.window.has_older);
    let latest_sequence = latest.events[0].sequence;
    let older = projection_history_page(
        &state,
        client,
        session.id,
        request(
            ProjectionWindowAnchor::BeforeSequence(latest_sequence),
            ProjectionWindowDirection::Backward,
        ),
    )
    .await
    .expect("older page");
    assert_eq!(older.events.len(), 1);
    assert!(older.events[0].sequence < latest_sequence);
    let newer = projection_history_page(
        &state,
        client,
        session.id,
        request(
            ProjectionWindowAnchor::AfterSequence(older.events[0].sequence),
            ProjectionWindowDirection::Forward,
        ),
    )
    .await
    .expect("newer page");
    assert_eq!(newer.events, latest.events);
    let mut invalid_request = request(
        ProjectionWindowAnchor::AfterSequence(older.events[0].sequence),
        ProjectionWindowDirection::Forward,
    );
    invalid_request.direction = ProjectionWindowDirection::Backward;
    let invalid = projection_history_page(&state, client, session.id, invalid_request).await;
    assert!(invalid.is_err(), "invalid direction must fail closed");
    let missing_id = bcode_session_models::SessionId::new();
    assert!(
        projection_history_page(
            &state,
            client,
            missing_id,
            request(
                ProjectionWindowAnchor::Latest,
                ProjectionWindowDirection::Backward
            ),
        )
        .await
        .is_err()
    );
    drop(state);
    assert!(!root.path().join(missing_id.to_string()).exists());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn projection_history_pages_survive_client_reconnect_over_real_ipc() {
    use bcode_session_models::{
        ProjectionWindowAnchor, ProjectionWindowDirection, ProjectionWindowLimits,
        ProjectionWindowRequest, ProjectionWindowTarget, SessionEventKind, SessionProjectionKind,
    };

    let root = tempfile::tempdir().expect("session root");
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
    let session = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .expect("session");
    let client_id = bcode_session_models::ClientId::new();
    let oldest = sessions
        .append_user_message(session.id, client_id, "oldest".to_owned())
        .await
        .expect("oldest")
        .last()
        .expect("oldest event")
        .sequence;
    for _ in 0..37 {
        sessions
            .append_event(session.id, SessionEventKind::ClientAttached { client_id })
            .await
            .expect("non-presenting event");
    }
    let newest = sessions
        .append_user_message(session.id, client_id, "newest".to_owned())
        .await
        .expect("newest")
        .last()
        .expect("newest event")
        .sequence;
    let state = std::sync::Arc::new(crate::tests::test_server_state(sessions));
    let socket_dir = tempfile::tempdir().expect("socket directory");
    let request = |anchor, direction| ProjectionWindowRequest {
        projection: SessionProjectionKind::Transcript,
        anchor,
        direction,
        target: ProjectionWindowTarget {
            min_items: Some(1),
            width_columns: Some(80),
            ..ProjectionWindowTarget::default()
        },
        limits: ProjectionWindowLimits {
            max_items: 1,
            max_events_scanned: 5,
            max_bytes: 4096,
        },
    };
    let mut cursor = newest;
    let mut empty_pages = 0;
    for connection_index in 0..2 {
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(
            socket_dir
                .path()
                .join(format!("server-{connection_index}.sock")),
        );
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("IPC listener");
        let serving = std::sync::Arc::clone(&state);
        let server = tokio::spawn(async move {
            loop {
                let stream = listener.accept().await.expect("client connection");
                let serving = std::sync::Arc::clone(&serving);
                tokio::spawn(async move {
                    crate::handle_client(stream, serving)
                        .await
                        .expect("serve client");
                });
            }
        });
        let client = bcode_client::BcodeClient::new(endpoint);
        while cursor > oldest {
            let page = client
                .session_projection_history_page(
                    session.id,
                    request(
                        ProjectionWindowAnchor::BeforeSequence(cursor),
                        ProjectionWindowDirection::Backward,
                    ),
                )
                .await
                .expect("typed IPC history page");
            let range = page.window.source_range.expect("source range");
            assert!(range.end_sequence < cursor);
            assert!(page.events.len() <= 5);
            if page.window.transcript_items.is_empty() {
                empty_pages += 1;
            }
            cursor = range.start_sequence;
            if connection_index == 0 && empty_pages == 2 {
                break;
            }
        }
        drop(client);
        server.abort();
        assert!(server.await.expect_err("listener aborted").is_cancelled());
    }
    assert_eq!(cursor, oldest);
    assert!(empty_pages > 2);
    drop(state);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn projection_history_pages_cross_non_presenting_gaps_with_bounded_progress() {
    use bcode_session_models::{
        ProjectionWindowAnchor, ProjectionWindowDirection, ProjectionWindowLimits,
        ProjectionWindowRequest, ProjectionWindowTarget, SessionEventKind, SessionProjectionKind,
    };

    let root = tempfile::tempdir().expect("root");
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
    let session = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .expect("session");
    let client = bcode_session_models::ClientId::new();
    let oldest = sessions
        .append_user_message(session.id, client, "oldest".to_owned())
        .await
        .expect("oldest message")
        .last()
        .expect("message event")
        .sequence;
    for _ in 0..37 {
        sessions
            .append_event(
                session.id,
                SessionEventKind::ClientAttached { client_id: client },
            )
            .await
            .expect("non-presenting event");
    }
    let newest = sessions
        .append_user_message(session.id, client, "newest".to_owned())
        .await
        .expect("newest message")
        .last()
        .expect("message event")
        .sequence;
    let state = crate::tests::test_server_state(sessions);
    let request = |anchor, direction| ProjectionWindowRequest {
        projection: SessionProjectionKind::Transcript,
        anchor,
        direction,
        target: ProjectionWindowTarget {
            min_items: Some(1),
            width_columns: Some(80),
            ..ProjectionWindowTarget::default()
        },
        limits: ProjectionWindowLimits {
            max_items: 1,
            max_events_scanned: 5,
            max_bytes: 4096,
        },
    };
    let mut cursor = newest;
    let mut empty_pages = 0;
    while cursor > oldest {
        let page = projection_history_page(
            &state,
            client,
            session.id,
            request(
                ProjectionWindowAnchor::BeforeSequence(cursor),
                ProjectionWindowDirection::Backward,
            ),
        )
        .await
        .expect("bounded older page");
        let range = page.window.source_range.expect("progress interval");
        assert!(page.events.len() <= 5);
        assert!(range.end_sequence < cursor);
        assert_eq!(
            page.events.first().map(|event| event.sequence),
            Some(range.start_sequence)
        );
        if page.window.transcript_items.is_empty() {
            empty_pages += 1;
        }
        cursor = range.start_sequence;
    }
    assert_eq!(cursor, oldest);
    assert!(empty_pages > 1);

    let mut cursor = oldest;
    let mut empty_pages = 0;
    while cursor < newest {
        let page = projection_history_page(
            &state,
            client,
            session.id,
            request(
                ProjectionWindowAnchor::AfterSequence(cursor),
                ProjectionWindowDirection::Forward,
            ),
        )
        .await
        .expect("bounded newer page");
        let range = page.window.source_range.expect("progress interval");
        assert!(page.events.len() <= 5);
        assert!(range.start_sequence > cursor);
        if page.window.transcript_items.is_empty() {
            empty_pages += 1;
        }
        cursor = range.end_sequence;
    }
    assert_eq!(cursor, newest);
    assert!(empty_pages > 1);
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
    let registry = bcode_session::storage_admission::StorageAdmissionRegistry::open(root.path())
        .expect("registry");
    *state
        .storage_daemon_registration
        .lock()
        .expect("registration lock") = Some(
        registry
            .register_daemon(bcode_session_models::SessionId::new())
            .expect("register"),
    );
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
    assert!(
        !state
            .storage_daemon_registration
            .lock()
            .expect("registration lock")
            .as_ref()
            .expect("registered")
            .healthy()
    );
    assert!(
        state
            .storage_tracking_failed
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    drop(state);
    assert!(registry.admit_maintenance(32).is_err());
    assert_eq!(repeated.events.len(), page.events.len());
    assert_eq!(std::fs::read(&path).expect("preserved"), b"damaged");
}
