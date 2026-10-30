//! Behavioral adapter tests. Deliberately do not inspect source text or implementation names.

use super::*;
use axum::body::Body;
use bcode_session_models::SessionId;

// Test credential only; never used for a listener outside these tests.
const TOKEN: &str = "test-only-credential-not-a-secret-0123456789abcdef";
const AUTHORITY: &str = "127.0.0.1:32123";

fn access() -> XrayHttpAccess {
    let grant = XrayAccessGrant::authorized(BTreeSet::from([SessionId::new()]), false).unwrap();
    XrayHttpAccess::new(TOKEN, grant).unwrap()
}

fn request(uri: &str) -> Request {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, AUTHORITY)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap()
}

#[test]
fn lifetime_is_optional_and_has_no_twenty_four_hour_policy_ceiling() {
    let mut config = XrayHttpConfig::default();
    assert!(config.max_observation_duration.is_none());
    assert!(config.validate().is_ok());
    config.max_observation_duration = Some(Duration::from_hours(7 * 24));
    assert!(config.validate().is_ok());
    config.max_observation_duration = Some(Duration::ZERO);
    assert!(config.validate().is_err());
}

#[test]
fn bearer_is_required_and_duplicate_authorization_fails_closed() {
    let access = access();
    let config = XrayHttpConfig::default();
    let mut input = request("/v1/xray/observe");
    assert!(request_guard(&access, &config, AUTHORITY, &input).is_ok());
    input.headers_mut().remove(header::AUTHORIZATION);
    assert_eq!(
        request_guard(&access, &config, AUTHORITY, &input)
            .unwrap_err()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    input.headers_mut().append(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer wrong"),
    );
    input.headers_mut().append(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer other"),
    );
    assert_eq!(
        request_guard(&access, &config, AUTHORITY, &input)
            .unwrap_err()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn origins_are_exact_explicit_and_noncredentialed() {
    let access = access();
    let mut config = XrayHttpConfig::default();
    let mut input = request("/v1/xray/observe");
    input.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("http://localhost:8000"),
    );
    assert_eq!(
        request_guard(&access, &config, AUTHORITY, &input)
            .unwrap_err()
            .status(),
        StatusCode::FORBIDDEN
    );
    config
        .allowed_origins
        .insert("http://localhost:8000".into());
    assert_eq!(
        request_guard(&access, &config, AUTHORITY, &input).unwrap(),
        Some("http://localhost:8000".into())
    );
    input.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("http://localhost:8001"),
    );
    assert!(request_guard(&access, &config, AUTHORITY, &input).is_err());
    for origin in [
        "null",
        "*",
        "https://example.com",
        "http://localhost:8000/",
        "http://user@localhost:8000",
        "http://localhost:8000?token=private",
    ] {
        assert!(!valid_origin(origin));
    }
    assert!(valid_origin("http://127.0.0.1:8000"));
    assert!(valid_origin("http://[::1]:8000"));
}

#[test]
fn queries_replay_cookies_and_foreign_hosts_are_rejected() {
    let access = access();
    let config = XrayHttpConfig::default();
    assert!(
        request_guard(
            &access,
            &config,
            AUTHORITY,
            &request("/v1/xray/observe?token=private")
        )
        .is_err()
    );
    for (name, value) in [
        ("last-event-id", "42"),
        ("cookie", "session=private"),
        ("host", "attacker.example"),
    ] {
        let mut input = request("/v1/xray/observe");
        input.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        assert_eq!(
            request_guard(&access, &config, AUTHORITY, &input)
                .unwrap_err()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}

#[test]
fn browser_fetch_cannot_bypass_missing_origin() {
    let access = access();
    let config = XrayHttpConfig::default();
    let mut input = request("/v1/xray/observe");
    input
        .headers_mut()
        .insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
    assert_eq!(
        request_guard(&access, &config, AUTHORITY, &input)
            .unwrap_err()
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn bounded_json_parser_rejects_unknown_fields_oversize_and_wrong_content_type() {
    let mut input = request("/v1/xray/observe");
    *input.body_mut() = Body::from(
        r#"{"schema_version":1,"session_id":"s","deep_capture":false,"control":"cancel"}"#,
    );
    assert_eq!(
        request_json::<XrayObserveRequest>(input)
            .await
            .unwrap_err()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let mut input = request("/v1/xray/observe");
    *input.body_mut() = Body::from(vec![b'x'; MAX_REQUEST_BYTES + 1]);
    assert_eq!(
        request_json::<XrayObserveRequest>(input)
            .await
            .unwrap_err()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let mut input = request("/v1/xray/observe");
    input
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    assert_eq!(
        request_json::<XrayObserveRequest>(input)
            .await
            .unwrap_err()
            .status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
}

#[test]
fn response_serialization_is_bounded_and_sensitive_responses_are_not_cacheable() {
    assert!(bounded_json(&"x".repeat(MAX_FRAME_BYTES + 1)).is_err());
    let value = serde_json::json!({"future_event": "a\nb", "unknown": [1, 2]});
    let encoded = bounded_json(&value).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&encoded).unwrap(),
        value
    );
    let response = secure_headers(public_error(StatusCode::FORBIDDEN, "xray_forbidden"), None);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );
}

#[tokio::test]
async fn listener_defaults_to_loopback_and_rejects_remote_binding() {
    let mut config = XrayHttpConfig::default();
    let listener = bind(&config).await.unwrap();
    assert!(listener.local_addr().unwrap().ip().is_loopback());
    config.bind = "0.0.0.0:0".parse().unwrap();
    assert!(matches!(
        bind(&config).await,
        Err(XrayHttpStartError::InvalidConfiguration)
    ));
}

#[tokio::test]
async fn engine_ready_and_finalization_remain_explicit_diagnostic_boundaries() {
    let hub = bcode_xray::XrayHub::default();
    let session_id = SessionId::new().to_string();
    let mut observation = hub
        .observe(XrayObserveRequest {
            schema_version: 1,
            session_id: session_id.clone(),
            deep_capture: false,
        })
        .unwrap();
    let ready = observation.recv().await.unwrap();
    assert_eq!(ready.event.name, "ready");
    let encoded = bounded_json(&ready).unwrap();
    let decoded: bcode_xray_models::XrayEnvelope = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, ready);
    observation.finalize().unwrap();
    let finalized = observation.recv().await.unwrap();
    assert_eq!(finalized.event.name, "finalized");
    assert!(observation.recv().await.is_none());
    drop(observation);
    assert!(!hub.capture_active(&session_id));
}

#[tokio::test]
async fn sse_delivers_ready_and_finalized_without_replay_ids() {
    let hub = bcode_xray::XrayHub::default();
    let session_id = SessionId::new().to_string();
    let observation = hub
        .observe(XrayObserveRequest {
            schema_version: 1,
            session_id: session_id.clone(),
            deep_capture: false,
        })
        .unwrap();
    let calls = Arc::new(Semaphore::new(1));
    let (shutdown, stopped) = watch::channel(false);
    let response = observation_response(
        observation,
        Arc::clone(&calls).try_acquire_owned().unwrap(),
        stopped,
        Duration::from_secs(60),
    );
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    // Shutdown is independent of execution and HTTP body polling.
    shutdown.send(true).unwrap();
    let bytes = timeout(
        Duration::from_secs(2),
        to_bytes(response.into_body(), MAX_FRAME_BYTES),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("event: xray"));
    assert!(text.contains("\"name\":\"ready\""));
    assert!(text.contains("\"name\":\"finalized\""));
    assert!(!text.lines().any(|line| line.starts_with("id:")));
    timeout(Duration::from_secs(2), async {
        while hub.capture_active(&session_id) || calls.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(calls);
}

#[tokio::test]
async fn dropping_body_releases_only_diagnostics_and_capacity() {
    let hub = bcode_xray::XrayHub::default();
    let session_id = SessionId::new().to_string();
    let observation = hub
        .observe(XrayObserveRequest {
            schema_version: 1,
            session_id: session_id.clone(),
            deep_capture: true,
        })
        .unwrap();
    let calls = Arc::new(Semaphore::new(1));
    let (_shutdown, stopped) = watch::channel(false);
    let response = observation_response(
        observation,
        Arc::clone(&calls).try_acquire_owned().unwrap(),
        stopped,
        Duration::from_secs(60),
    );
    assert!(hub.deep_capture_active(&session_id));
    drop(response);
    // Task abortion is asynchronous; observe retirement, not just an abort request.
    timeout(Duration::from_secs(2), async {
        while hub.capture_active(&session_id) || calls.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(calls);
}

#[tokio::test(start_paused = true)]
async fn stalled_reader_cannot_retain_capture_indefinitely() {
    let hub = bcode_xray::XrayHub::default();
    let session_id = SessionId::new().to_string();
    let observation = hub
        .observe(XrayObserveRequest {
            schema_version: 1,
            session_id: session_id.clone(),
            deep_capture: true,
        })
        .unwrap();
    let calls = Arc::new(Semaphore::new(1));
    let (_shutdown, stopped) = watch::channel(false);
    let response = observation_response(
        observation,
        Arc::clone(&calls).try_acquire_owned().unwrap(),
        stopped,
        Duration::from_secs(1),
    );
    // Do not poll the HTTP body: cutoff and send timeout must still retire the observer.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(REQUEST_TIMEOUT + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(!hub.capture_active(&session_id));
    // Finalized retention is bounded, even when no explicit release arrives.
    tokio::time::advance(FINALIZED_PAYLOAD_IDLE + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(calls.available_permits(), 1);
    drop(calls);
    drop(response);
}
