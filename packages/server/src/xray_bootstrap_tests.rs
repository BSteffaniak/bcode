//! Bootstrap behavioral coverage. Written during interface convergence; not executed.
use super::*;

#[test]
fn endpoint_expiry_uses_only_its_own_http_registrations() {
    let session = SessionId::new();
    let hub = bcode_xray::XrayHub::default();
    let unrelated_observer = hub
        .observe(bcode_xray_models::XrayObserveRequest {
            schema_version: 1,
            session_id: session.to_string(),
            deep_capture: false,
        })
        .unwrap();
    let grant = XrayAccessGrant::authorized(BTreeSet::from([session]), false).unwrap();
    let unused_endpoint = XrayHttpAccess::new(&new_token(), grant).unwrap();
    let start = Instant::now();
    let mut last_active = start;
    assert!(
        hub.session_observation_active(&session.to_string())
            .unwrap()
    );
    assert!(unused_endpoint_expired(
        &mut last_active,
        unused_endpoint.active_count(),
        start + UNUSED_TIMEOUT,
    ));
    drop(unrelated_observer);
}

#[test]
fn active_idle_streams_and_draining_streams_never_expire() {
    let start = Instant::now();
    let mut last_active = start;
    // Count includes both live idle SSE and finalized streams draining payloads.
    // Lack of model/tool activity is intentionally irrelevant to this policy.
    let far_later = start + Duration::from_hours(7 * 24);
    assert!(!unused_endpoint_expired(&mut last_active, 1, far_later));
    assert!(!unused_endpoint_expired(
        &mut last_active,
        0,
        far_later + UNUSED_TIMEOUT / 2
    ));
    assert!(unused_endpoint_expired(
        &mut last_active,
        0,
        far_later + UNUSED_TIMEOUT
    ));
    // A poisoned registry conservatively reports usize::MAX, never unused.
    assert!(!unused_endpoint_expired(
        &mut last_active,
        usize::MAX,
        far_later + UNUSED_TIMEOUT
    ));
}

#[test]
fn credentials_are_independent_and_not_debug_exposed() {
    let first = new_token();
    let second = new_token();
    assert_eq!(first.len(), 96);
    assert_ne!(*first, *second);
    assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let info = XrayEndpointInfo {
        endpoint_id: "test-endpoint".into(),
        session_id: SessionId::new(),
        endpoint: "http://127.0.0.1:12345".into(),
        credential_file: PathBuf::from("/private/diagnostic-capability"),
        deep_capture: true,
        schema_version: 1,
    };
    let debug = format!("{info:?}");
    assert!(!debug.contains("diagnostic-capability"));
    assert!(!debug.contains(first.as_str()));
}

#[cfg(unix)]
#[test]
fn credential_file_is_private_exclusive_confined_and_removed_on_drop() {
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let token = new_token();
    let credential = CredentialFile::create(parent.path(), &id, &token).unwrap();
    assert_eq!(
        std::fs::metadata(&credential.root)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&credential.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::read_to_string(&credential.path).unwrap(),
        token.as_str()
    );
    assert_eq!(credential.path.canonicalize().unwrap(), credential.path);
    assert!(
        credential
            .path
            .starts_with(parent.path().canonicalize().unwrap())
    );
    assert!(CredentialFile::create(parent.path(), &id, "replacement").is_err());
    assert_eq!(
        std::fs::read_to_string(&credential.path).unwrap(),
        token.as_str()
    );
    let path = credential.path.clone();
    let root = credential.root.clone();
    drop(credential);
    assert!(!path.exists());
    assert!(!root.exists());
}

#[cfg(unix)]
#[test]
fn preexisting_symlink_is_not_followed_or_removed() {
    use std::os::unix::fs::symlink;
    let parent = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let link = parent.path().join(format!(".xray-{id}"));
    symlink(outside.path(), &link).unwrap();
    assert!(CredentialFile::create(parent.path(), &id, &new_token()).is_err());
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!outside.path().join("credential").exists());
}

#[tokio::test]
async fn bootstrap_requires_verified_client_before_touching_storage() {
    let state = Arc::new(crate::tests::test_server_state(
        bcode_session::SessionManager::default(),
    ));
    let caller = ClientId::new();
    state.register_client(caller).await;
    assert!(matches!(
        state
            .open_xray_endpoint(caller, SessionId::new(), true)
            .await,
        Err(XrayBootstrapError::Forbidden)
    ));
    assert!(!state.xray_endpoints.active().await);
    assert!(matches!(
        state.close_xray_endpoint(caller, "unknown").await,
        Err(XrayBootstrapError::Forbidden)
    ));
    drop(state);
}

#[tokio::test]
async fn idle_observation_blocks_daemon_shutdown_without_an_ipc_client() {
    let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    let observation = state
        .xray
        .observe(bcode_xray_models::XrayObserveRequest {
            schema_version: 1,
            session_id: SessionId::new().to_string(),
            deep_capture: false,
        })
        .unwrap();
    assert!(
        state
            .idle_shutdown_blocker()
            .await
            .unwrap()
            .contains("X-ray")
    );
    drop(state);
    drop(observation);
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_close_is_owner_scoped_and_joins_cleanup() {
    let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    let owner = ClientId::new();
    let foreign = ClientId::new();
    for caller in [owner, foreign] {
        state.register_client(caller).await;
        state
            .set_client_session_namespace(caller, bcode_ipc::daemon_namespace())
            .await;
    }
    let root = tempfile::tempdir().unwrap();
    let credential = CredentialFile::create(root.path(), "close-test", &new_token()).unwrap();
    let path = credential.path.clone();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _credential = credential;
        let _ = stopped.await;
    });
    state
        .xray_endpoints
        .entries
        .lock()
        .await
        .insert("close-test".into(), Endpoint { owner, stop, task });
    assert!(
        state
            .close_xray_endpoint(foreign, "close-test")
            .await
            .is_err()
    );
    assert!(path.exists());
    assert!(state.xray_endpoints.active().await);
    state
        .close_xray_endpoint(owner, "close-test")
        .await
        .unwrap();
    assert!(!path.exists());
    assert!(!state.xray_endpoints.active().await);
    drop(state);
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_revokes_and_removes_all_credential_files() {
    let registry = XrayEndpointRegistry::default();
    let parent = tempfile::tempdir().unwrap();
    let credential = CredentialFile::create(parent.path(), "shutdown-test", &new_token()).unwrap();
    let path = credential.path.clone();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _credential = credential;
        let _ = stopped.await;
    });
    registry.entries.lock().await.insert(
        "shutdown-test".into(),
        Endpoint {
            owner: ClientId::new(),
            stop,
            task,
        },
    );
    registry.stop_all().await;
    assert!(!path.exists());
    assert!(!registry.active().await);
}

#[cfg(unix)]
#[tokio::test]
async fn real_bootstrap_serves_only_granted_session_and_mode_then_revokes() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new({
        let mut server = crate::tests::test_server_state(bcode_session::SessionManager::default());
        server.state_root = root.path().to_path_buf();
        server
    });
    let session = state
        .sessions
        .create_session(None, PathBuf::from("/"))
        .await
        .unwrap();
    let owner = ClientId::new();
    state.register_client(owner).await;
    state
        .set_client_session_namespace(owner, bcode_ipc::daemon_namespace())
        .await;
    let info = state
        .open_xray_endpoint(owner, session.id, false)
        .await
        .unwrap();
    let token = Zeroizing::new(std::fs::read_to_string(&info.credential_file).unwrap());
    let address = info.endpoint.strip_prefix("http://").unwrap();
    for (selected, deep, expected) in [
        (session.id, false, "HTTP/1.1 200"),
        (SessionId::new(), false, "HTTP/1.1 403"),
        (session.id, true, "HTTP/1.1 403"),
    ] {
        let body = serde_json::to_string(&bcode_xray_models::XrayObserveRequest {
            schema_version: 1,
            session_id: selected.to_string(),
            deep_capture: deep,
        })
        .unwrap();
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(format!(
            "POST /v1/xray/observe HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
            token.as_str(), body.len(),
        ).as_bytes()).await.unwrap();
        let mut reader = BufReader::new(socket);
        let mut status = String::new();
        timeout(Duration::from_secs(3), reader.read_line(&mut status))
            .await
            .unwrap()
            .unwrap();
        assert!(status.starts_with(expected), "unexpected response status");
        // Socket drop releases this observer without affecting session work.
    }
    state
        .close_xray_endpoint(owner, &info.endpoint_id)
        .await
        .unwrap();
    drop(state);
    assert!(!info.credential_file.exists());
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

#[test]
fn bootstrap_ipc_round_trips_explicit_scope() {
    let session_id = SessionId::new();
    let request = bcode_ipc::Request::OpenXrayEndpoint {
        session_id,
        deep_capture: true,
    };
    let bytes = bcode_ipc::encode_request(&request).unwrap();
    assert_eq!(bcode_ipc::decode_request(&bytes).unwrap(), request);
}
