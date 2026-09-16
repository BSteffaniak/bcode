use super::*;

#[tokio::test]
async fn unrelated_daemon_does_not_block_session_sweep() {
    let root = tempfile::tempdir().expect("root");
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("sessions");
    let id = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .expect("create")
        .id;
    sessions
        .release_session_ownership(id)
        .await
        .expect("release");
    let state = crate::tests::test_server_state(sessions);
    crate::storage_read_admission::register_startup(&state, root.path().to_path_buf()).await;
    let registry = bcode_session::storage_admission::StorageAdmissionRegistry::open(root.path())
        .expect("registry");
    let foreign = registry
        .register_daemon(bcode_session_models::SessionId::new())
        .expect("foreign");
    let unrelated = bcode_session::storage_admission::StorageAdmissionRegistry::open_session(
        root.path(),
        bcode_session_models::SessionId::new(),
    )
    .expect("unrelated registry");
    let unrelated_reader = unrelated
        .admit_read(bcode_session_models::SessionId::new())
        .expect("unrelated read");
    let scoped =
        bcode_session::storage_admission::StorageAdmissionRegistry::open_session(root.path(), id)
            .expect("scoped");
    drop(
        scoped
            .admit_read(bcode_session_models::SessionId::new())
            .expect("abandoned read"),
    );
    assert!(scoped.admit_maintenance(4096).is_err());
    let request = StorageCompressionRequest {
        session_id: id,
        as_of_ms: crate::current_time_ms(),
        minimum_age_ms: None,
        tier: StorageCompressionTier::Light,
        dry_run: false,
        cursor: None,
    };
    let blocked = compress_page(&state, request.clone())
        .await
        .expect("blocked");
    assert_eq!(blocked.failure, None);
    assert_eq!(blocked.disposition, Disposition::Processed);
    assert_eq!(blocked.failures, 0);
    drop(
        scoped
            .admit_maintenance(4096)
            .expect("manual compression recovered abandoned reader"),
    );
    assert!(blocked.next.is_some());
    drop(unrelated_reader);
    foreign.finish().expect("clean foreign shutdown");
    let retry = compress_page(&state, request).await.expect("retry");
    drop(state);
    assert_eq!(retry.failure, None);
    assert_eq!(retry.disposition, Disposition::Processed);
}
