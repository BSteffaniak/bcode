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
fn origin_grants_accept_only_exact_loopback_http_origins() {
    for origin in [
        "http://localhost",
        "http://localhost:5173",
        "http://127.0.0.1:5173",
        "http://127.42.0.1:5173",
        "http://[::1]:5173",
    ] {
        let grant = XrayOriginGrant::new(origin, 60).unwrap();
        assert_eq!(grant.origin, origin);
        assert!(require_unexpired_origin(Some(grant.expires_at)).is_ok());
    }
    for origin in [
        "",
        "*",
        "null",
        "https://localhost:5173",
        "file:///tmp/viewer",
        "http://0.0.0.0:5173",
        "http://192.168.1.1:5173",
        "http://[::]:5173",
        "http://example.com",
        "http://localhost.example.com",
        "http://localhost.",
        "http://127.0.0.1.example.com",
        "http://2130706433",
        "http://127.1",
        "http://LOCALHOST:5173",
        "http://localhost:80",
        "http://localhost:05173",
        "http://localhost:5173/",
        "http://localhost:5173/path",
        "http://localhost:5173?query",
        "http://localhost:5173#fragment",
        "http://user@localhost:5173",
        "http://user:password@localhost:5173",
        "http://localhost:5173 http://127.0.0.1:5173",
        " http://localhost:5173",
        "http://localhost:5173\n",
        "http://localhost:5173\r\nOrigin: http://example.com",
    ] {
        assert!(
            matches!(
                XrayOriginGrant::new(origin, 60),
                Err(XrayBootstrapError::InvalidOriginGrant)
            ),
            "accepted invalid Origin"
        );
    }
    assert!(XrayOriginGrant::new(&"x".repeat(257), 60).is_err());
    assert!(XrayOriginGrant::new("http://localhost:5173", 0).is_err());
}

#[tokio::test(start_paused = true)]
async fn origin_expiry_is_absolute_and_absent_grants_have_no_deadline() {
    let grant = XrayOriginGrant::new("http://localhost:5173", 2).unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(
        require_unexpired_origin(Some(grant.expires_at)),
        Err(XrayBootstrapError::InvalidOriginGrant)
    ));
    origin_expired(Some(grant.expires_at)).await;
    assert!(
        timeout(Duration::from_secs(1), origin_expired(None))
            .await
            .is_err()
    );
    assert!(require_unexpired_origin(None).is_ok());
}

#[tokio::test]
async fn origin_grant_never_bypasses_bootstrap_authorization() {
    let state = Arc::new(crate::tests::test_server_state(
        bcode_session::SessionManager::default(),
    ));
    let grant = XrayOriginGrant::new("http://localhost:5173", 60).unwrap();
    assert!(matches!(
        state
            .open_xray_endpoint_with_origin(ClientId::new(), SessionId::new(), true, Some(grant),)
            .await,
        Err(XrayBootstrapError::Forbidden)
    ));
    assert!(!state.xray_endpoints.active().await);
    drop(state);
}

#[cfg(unix)]
async fn origin_test_server() -> (tempfile::TempDir, Arc<ServerState>, ClientId, SessionId) {
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
    (root, state, owner, session.id)
}

#[cfg(unix)]
async fn origin_test_request(
    endpoint: &XrayEndpointInfo,
    origin_headers: &str,
    token: &str,
    session: SessionId,
    deep_capture: bool,
) -> (String, tokio::io::BufReader<tokio::net::TcpStream>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let address = endpoint.endpoint.strip_prefix("http://").unwrap();
    let body = serde_json::to_string(&bcode_xray_models::XrayObserveRequest {
        schema_version: 1,
        session_id: session.to_string(),
        deep_capture,
    })
    .unwrap();
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket.write_all(format!(
        "POST /v1/xray/observe HTTP/1.1\r\nHost: {address}\r\n{origin_headers}Authorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
        body.len(),
    ).as_bytes()).await.unwrap();
    let mut reader = BufReader::new(socket);
    let headers = timeout(Duration::from_secs(3), async {
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            assert!(headers.len() + line.len() <= 16 * 1024);
            if line == "\r\n" {
                break;
            }
            headers.push_str(&line);
        }
        headers
    })
    .await
    .unwrap();
    (headers, reader)
}

#[cfg(unix)]
#[tokio::test]
async fn browser_grant_is_exact_requires_bearer_and_preserves_session_and_deep_scope() {
    let (_root, state, owner, session) = origin_test_server().await;
    let endpoint = state
        .open_xray_endpoint_with_options(
            owner,
            session,
            false,
            XrayEndpointOptions {
                origin_grant: Some(bcode_ipc::XrayOriginGrantRequest {
                    origin: "http://localhost:5173".into(),
                    lifetime_seconds: 60,
                }),
            },
        )
        .await
        .unwrap();
    let token = Zeroizing::new(std::fs::read_to_string(&endpoint.credential_file).unwrap());
    let exact = "Origin: http://localhost:5173\r\n";
    for (headers, bearer, selected, deep, status, cors) in [
        (exact, token.as_str(), session, false, "200", true),
        (exact, "wrong-bearer", session, false, "401", false),
        (exact, token.as_str(), SessionId::new(), false, "403", true),
        (exact, token.as_str(), session, true, "403", true),
        (
            "Origin: http://localhost:5174\r\n",
            token.as_str(),
            session,
            false,
            "403",
            false,
        ),
        (
            "Origin: http://127.0.0.1:5173\r\n",
            token.as_str(),
            session,
            false,
            "403",
            false,
        ),
        (
            "Origin: null\r\n",
            token.as_str(),
            session,
            false,
            "403",
            false,
        ),
        (
            "Origin: *\r\n",
            token.as_str(),
            session,
            false,
            "403",
            false,
        ),
        (
            "Origin: http://localhost:5173\r\nOrigin: http://localhost:5173\r\n",
            token.as_str(),
            session,
            false,
            "403",
            false,
        ),
    ] {
        let (response, _stream) =
            origin_test_request(&endpoint, headers, bearer, selected, deep).await;
        assert!(response.starts_with(&format!("HTTP/1.1 {status}")));
        assert_eq!(
            response.contains("access-control-allow-origin: http://localhost:5173\r\n"),
            cors
        );
        assert!(!response.contains("access-control-allow-origin: *"));
        assert!(!response.contains("access-control-allow-credentials"));
    }
    let other_endpoint = state
        .open_xray_endpoint_with_origin(
            owner,
            session,
            false,
            Some(XrayOriginGrant::new("http://localhost:5173", 60).unwrap()),
        )
        .await
        .unwrap();
    let (response, _stream) =
        origin_test_request(&other_endpoint, exact, &token, session, false).await;
    assert!(response.starts_with("HTTP/1.1 401"));
    // Revocation is scoped to the capability, not the shared Origin or session.
    state
        .close_xray_endpoint(owner, &endpoint.endpoint_id)
        .await
        .unwrap();
    assert!(!endpoint.credential_file.exists());
    let other_token =
        Zeroizing::new(std::fs::read_to_string(&other_endpoint.credential_file).unwrap());
    let (response, _stream) =
        origin_test_request(&other_endpoint, exact, &other_token, session, false).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    state
        .close_xray_endpoint(owner, &other_endpoint.endpoint_id)
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn expired_browser_grant_is_rejected_before_storage_or_observation() {
    let (root, state, owner, session) = origin_test_server().await;
    let before = std::fs::read_dir(root.path()).unwrap().count();
    let expired = XrayOriginGrant {
        origin: "http://localhost:5173".into(),
        expires_at: Instant::now(),
    };
    assert!(matches!(
        state
            .open_xray_endpoint_with_origin(owner, session, true, Some(expired))
            .await,
        Err(XrayBootstrapError::InvalidOriginGrant)
    ));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), before);
    assert!(!state.xray_endpoints.active().await);
    assert!(
        !state
            .xray
            .session_observation_active(&session.to_string())
            .unwrap()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn browser_preflight_is_exact_and_never_an_authentication_substitute() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_root, state, owner, session) = origin_test_server().await;
    let endpoint = state
        .open_xray_endpoint_with_origin(
            owner,
            session,
            false,
            Some(XrayOriginGrant::new("http://localhost:5173", 60).unwrap()),
        )
        .await
        .unwrap();
    let address = endpoint.endpoint.strip_prefix("http://").unwrap();
    for (origin, method, requested_headers, expected) in [
        (
            "http://localhost:5173",
            "POST",
            "authorization, content-type",
            "204",
        ),
        (
            "http://localhost:5174",
            "POST",
            "authorization, content-type",
            "403",
        ),
        (
            "http://localhost:5173",
            "DELETE",
            "authorization, content-type",
            "403",
        ),
        (
            "http://localhost:5173",
            "POST",
            "authorization, content-type, cookie",
            "403",
        ),
    ] {
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(format!(
            "OPTIONS /v1/xray/observe HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nAccess-Control-Request-Method: {method}\r\nAccess-Control-Request-Headers: {requested_headers}\r\nContent-Length: 0\r\n\r\n",
        ).as_bytes()).await.unwrap();
        let mut reader = BufReader::new(socket);
        let mut status = String::new();
        timeout(Duration::from_secs(3), reader.read_line(&mut status))
            .await
            .unwrap()
            .unwrap();
        assert!(status.starts_with(&format!("HTTP/1.1 {expected}")));
        assert!(
            !state
                .xray
                .session_observation_active(&session.to_string())
                .unwrap()
        );
    }
    let (response, _stream) = origin_test_request(
        &endpoint,
        "Origin: http://localhost:5173\r\n",
        "",
        session,
        false,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 401"));
    state
        .close_xray_endpoint(owner, &endpoint.endpoint_id)
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn browser_grant_expires_active_endpoint_without_revoking_another_capability() {
    use tokio::io::AsyncBufReadExt;
    let (_root, state, owner, session) = origin_test_server().await;
    let ordinary = state
        .open_xray_endpoint(owner, session, false)
        .await
        .unwrap();
    let endpoint = state
        .open_xray_endpoint_with_origin(
            owner,
            session,
            true,
            Some(XrayOriginGrant::new("http://localhost:5173", 2).unwrap()),
        )
        .await
        .unwrap();
    let token = Zeroizing::new(std::fs::read_to_string(&endpoint.credential_file).unwrap());
    let (headers, mut reader) = origin_test_request(
        &endpoint,
        "Origin: http://localhost:5173\r\n",
        &token,
        session,
        true,
    )
    .await;
    assert!(headers.starts_with("HTTP/1.1 200"));
    timeout(Duration::from_secs(1), async {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            if line.contains("\"name\":\"ready\"") {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(state.xray.deep_capture_active(&session.to_string()));
    let correlation = bcode_xray_models::XrayCorrelation {
        session_id: session.to_string(),
        ..bcode_xray_models::XrayCorrelation::default()
    };
    let retained = state
        .xray
        .capture_payload(&correlation, "text/plain", b"private diagnostic fixture")
        .unwrap();
    assert!(retained.is_some());
    // Keep the idle SSE alive: absolute grant expiry overrides active-count renewal.
    timeout(Duration::from_secs(6), async {
        while endpoint.credential_file.exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(!state.xray.deep_capture_active(&session.to_string()));
    assert!(
        state
            .xray
            .capture_payload(
                &correlation,
                "text/plain",
                b"must not be retained after expiry",
            )
            .unwrap()
            .is_none()
    );
    assert!(
        !state
            .xray
            .session_observation_active(&session.to_string())
            .unwrap()
    );
    assert!(
        tokio::net::TcpStream::connect(endpoint.endpoint.strip_prefix("http://").unwrap())
            .await
            .is_err()
    );
    let other_token = Zeroizing::new(std::fs::read_to_string(&ordinary.credential_file).unwrap());
    let (headers, _stream) = origin_test_request(
        &ordinary,
        "Origin: http://localhost:5173\r\n",
        &other_token,
        session,
        false,
    )
    .await;
    assert!(headers.starts_with("HTTP/1.1 403"));
    let (headers, _stream) = origin_test_request(&ordinary, "", &other_token, session, false).await;
    assert!(headers.starts_with("HTTP/1.1 200"));
    state
        .close_xray_endpoint(owner, &ordinary.endpoint_id)
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn exact_origin_explicit_revocation_closes_active_stream_and_preserves_other_capability() {
    use tokio::io::AsyncBufReadExt as _;
    let (_root, state, owner, session) = origin_test_server().await;
    let foreign = ClientId::new();
    state.register_client(foreign).await;
    state
        .set_client_session_namespace(foreign, bcode_ipc::daemon_namespace())
        .await;
    let ordinary = state
        .open_xray_endpoint(owner, session, false)
        .await
        .unwrap();
    let endpoint = state
        .open_xray_endpoint_with_origin(
            owner,
            session,
            true,
            Some(XrayOriginGrant::new("http://localhost:5173", 60).unwrap()),
        )
        .await
        .unwrap();
    let token = Zeroizing::new(std::fs::read_to_string(&endpoint.credential_file).unwrap());
    let (headers, mut reader) = origin_test_request(
        &endpoint,
        "Origin: http://localhost:5173\r\n",
        &token,
        session,
        true,
    )
    .await;
    assert!(headers.starts_with("HTTP/1.1 200"));
    timeout(Duration::from_secs(3), async {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            if line.contains("\"name\":\"ready\"") {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(state.xray.deep_capture_active(&session.to_string()));
    assert!(
        state
            .close_xray_endpoint(foreign, &endpoint.endpoint_id)
            .await
            .is_err()
    );
    assert!(endpoint.credential_file.exists());
    assert!(state.xray.deep_capture_active(&session.to_string()));
    state
        .close_xray_endpoint(owner, &endpoint.endpoint_id)
        .await
        .unwrap();
    assert!(!endpoint.credential_file.exists());
    assert!(!state.xray.deep_capture_active(&session.to_string()));
    assert!(!state.xray.capture_active(&session.to_string()));
    // Buffered SSE framing may remain; closing cannot keep an active socket or
    // imply an engine delivery cutoff. Drain only this fixture's bounded stream.
    timeout(Duration::from_secs(3), async {
        let mut total = 0;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    total += count;
                    assert!(total <= 128 * 1024);
                }
            }
        }
    })
    .await
    .expect("explicit close must end active HTTP observation");
    assert!(
        tokio::net::TcpStream::connect(endpoint.endpoint.strip_prefix("http://").unwrap())
            .await
            .is_err()
    );
    assert!(ordinary.credential_file.exists());
    let ordinary_token =
        Zeroizing::new(std::fs::read_to_string(&ordinary.credential_file).unwrap());
    let (headers, _) = origin_test_request(
        &ordinary,
        "Origin: http://localhost:5173\r\n",
        &ordinary_token,
        session,
        false,
    )
    .await;
    assert!(headers.starts_with("HTTP/1.1 403"));
    let (headers, stream) =
        origin_test_request(&ordinary, "", &ordinary_token, session, false).await;
    assert!(headers.starts_with("HTTP/1.1 200"));
    drop(stream);
    state
        .close_xray_endpoint(owner, &ordinary.endpoint_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn options_default_denies_browser_access_and_authorizes_before_validation() {
    assert!(XrayEndpointOptions::default().origin_grant.is_none());
    let state = Arc::new(crate::tests::test_server_state(
        bcode_session::SessionManager::default(),
    ));
    let result = state
        .open_xray_endpoint_with_options(
            ClientId::new(),
            SessionId::new(),
            false,
            XrayEndpointOptions {
                origin_grant: Some(bcode_ipc::XrayOriginGrantRequest {
                    origin: "*".into(),
                    lifetime_seconds: 0,
                }),
            },
        )
        .await;
    assert!(matches!(result, Err(XrayBootstrapError::Forbidden)));
    assert!(!state.xray_endpoints.active().await);
    drop(state);
}

#[cfg(unix)]
#[tokio::test]
async fn options_validate_origin_before_session_capture_or_credential_creation() {
    let (root, state, owner, session) = origin_test_server().await;
    let before = std::fs::read_dir(root.path()).unwrap().count();
    let result = state
        .open_xray_endpoint_with_options(
            owner,
            session,
            true,
            XrayEndpointOptions {
                origin_grant: Some(bcode_ipc::XrayOriginGrantRequest {
                    origin: "http://remote.example".into(),
                    lifetime_seconds: 60,
                }),
            },
        )
        .await;
    assert!(matches!(
        result,
        Err(XrayBootstrapError::InvalidOriginGrant)
    ));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), before);
    assert!(!state.xray_endpoints.active().await);
    assert!(
        !state
            .xray
            .session_observation_active(&session.to_string())
            .unwrap()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn grant_expiry_revokes_slow_bodies_on_every_endpoint_route() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_root, state, owner, session) = origin_test_server().await;
    let endpoint = state
        .open_xray_endpoint_with_origin(
            owner,
            session,
            true,
            Some(XrayOriginGrant::new("http://localhost:5173", 2).unwrap()),
        )
        .await
        .unwrap();
    let token = Zeroizing::new(std::fs::read_to_string(&endpoint.credential_file).unwrap());
    let address = endpoint.endpoint.strip_prefix("http://").unwrap();
    let mut sockets = Vec::new();
    for route in ["observe", "payload", "finalize", "release"] {
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(format!(
            "POST /v1/xray/{route} HTTP/1.1\r\nHost: {address}\r\nOrigin: http://localhost:5173\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: 128\r\n\r\n{{",
            token.as_str(),
        ).as_bytes()).await.unwrap();
        sockets.push(socket);
    }
    for socket in sockets {
        let mut reader = BufReader::new(socket);
        let mut status = String::new();
        // Either fixed denial or connection close is fail-closed; never success.
        let _ = timeout(Duration::from_secs(6), reader.read_line(&mut status))
            .await
            .unwrap();
        assert!(!status.starts_with("HTTP/1.1 2"));
    }
    timeout(Duration::from_secs(3), async {
        while endpoint.credential_file.exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(!state.xray.deep_capture_active(&session.to_string()));
    assert!(!state.xray.capture_active(&session.to_string()));
}

#[cfg(unix)]
#[tokio::test]
async fn application_deadline_preserves_scope_policy_and_rejects_expired_capture() {
    let (_root, state, _owner, session) = origin_test_server().await;
    let application = XrayApplication::new(Arc::clone(&state), Arc::clone(&state.xray));
    let grant = XrayAccessGrant::authorized(BTreeSet::from([session]), true).unwrap();
    let request = |selected: SessionId| bcode_xray_models::XrayObserveRequest {
        schema_version: 1,
        session_id: selected.to_string(),
        deep_capture: true,
    };
    let expired = std::time::Instant::now();
    assert!(matches!(
        application
            .observe_with_deadline(&grant, request(SessionId::new()), Some(expired))
            .await,
        Err(crate::xray_application::XrayApplicationError::Forbidden)
    ));
    assert!(
        application
            .observe_with_deadline(&grant, request(session), Some(expired))
            .await
            .is_err()
    );
    assert!(!state.xray.deep_capture_active(&session.to_string()));
    let observation = application
        .observe_with_deadline(
            &grant,
            request(session),
            Some(std::time::Instant::now() + Duration::from_secs(60)),
        )
        .await
        .unwrap();
    assert!(state.xray.deep_capture_active(&session.to_string()));
    drop(observation);
    assert!(!state.xray.deep_capture_active(&session.to_string()));
    let observation = application.observe(&grant, request(session)).await.unwrap();
    drop(application);
    assert!(state.xray.deep_capture_active(&session.to_string()));
    drop(observation);
    drop(state);
}

#[test]
fn bootstrap_ipc_round_trips_explicit_scope() {
    let session_id = SessionId::new();
    let request = bcode_ipc::Request::OpenXrayEndpoint {
        session_id,
        deep_capture: true,
        origin_grant: None,
    };
    let bytes = bcode_ipc::encode_request(&request).unwrap();
    assert_eq!(bcode_ipc::decode_request(&bytes).unwrap(), request);
}
