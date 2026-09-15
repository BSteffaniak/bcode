use super::*;
mod admission;

#[tokio::test]
async fn manual_compression_ignores_scheduling_but_preserves_history_and_age_safety() {
    let root = tempfile::tempdir().expect("root");
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
    let id = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .expect("session")
        .id;
    sessions
        .append_event(
            id,
            bcode_session_models::SessionEventKind::SystemMessage {
                text: "manual compression 世界 ".repeat(20_000),
            },
        )
        .await
        .expect("append");
    let expected = sessions.session_history(id).await.expect("history");
    sessions
        .release_session_ownership(id)
        .await
        .expect("release");
    let mut state = crate::tests::test_server_state(sessions);
    state.startup_config.session_storage.enabled = false;
    crate::storage_read_admission::register_startup(&state, root.path().to_path_buf()).await;
    let mut request = StorageCompressionRequest {
        session_id: id,
        as_of_ms: crate::current_time_ms(),
        minimum_age_ms: Some(86_400_000),
        tier: StorageCompressionTier::Light,
        dry_run: true,
        cursor: None,
    };
    let access = root.path().join(id.to_string()).join("storage-access.bin");
    if access.exists() {
        std::fs::remove_file(&access).expect("remove tracking");
    }
    let db_path = root.path().join(id.to_string()).join("session.db");
    let before = std::fs::read(&db_path).expect("before");
    assert_eq!(
        compress_page(&state, request.clone())
            .await
            .expect("preview")
            .disposition,
        Disposition::UnknownAge
    );
    request.minimum_age_ms = None;
    assert_eq!(
        compress_page(&state, request.clone())
            .await
            .expect("preview")
            .disposition,
        Disposition::Eligible
    );
    assert_eq!(std::fs::read(&db_path).expect("unchanged"), before);
    assert!(!access.exists());
    request.dry_run = false;
    let mut saved = 0;
    loop {
        let result = compress_page(&state, request.clone()).await.expect("page");
        assert_eq!(result.disposition, Disposition::Processed);
        assert_eq!(result.failures, 0);
        saved += result.history_payload_bytes_saved;
        request.cursor = result.next;
        if request.cursor.is_none() {
            break;
        }
    }
    assert!(saved > 100_000);
    assert_eq!(
        state.sessions.session_history(id).await.expect("reopen"),
        expected
    );
    state
        .sessions
        .release_session_ownership(id)
        .await
        .expect("release");
    drop(state);
}

#[tokio::test]
async fn explicit_id_does_not_bypass_registered_reader() {
    let root = tempfile::tempdir().expect("root");
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
    let id = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .expect("session")
        .id;
    sessions
        .release_session_ownership(id)
        .await
        .expect("release");
    let state = crate::tests::test_server_state(sessions);
    crate::storage_read_admission::register_startup(&state, root.path().to_path_buf()).await;
    let registry =
        bcode_session::storage_admission::StorageAdmissionRegistry::open_session(root.path(), id)
            .expect("registry");
    let reader = registry
        .admit_read(bcode_session_models::SessionId::new())
        .expect("reader");
    let result = compress_page(
        &state,
        StorageCompressionRequest {
            session_id: id,
            as_of_ms: crate::current_time_ms(),
            minimum_age_ms: None,
            tier: StorageCompressionTier::Light,
            dry_run: false,
            cursor: Some(Cursor::History {
                start: 0,
                through: 0,
            }),
        },
    )
    .await
    .expect("result");
    drop(state);
    assert_eq!(result.failure, Some(Failure::AdmissionBusy));
    assert!(result.next.is_none());
    assert_eq!(result.disposition, Disposition::Unavailable);
    assert_eq!(result.failures, 1);
    drop(reader);
}
