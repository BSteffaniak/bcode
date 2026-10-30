use super::*;
use axum::body::Body;
use bcode_session_models::SessionId;

#[tokio::test]
async fn concurrent_finalize_is_one_cutoff_and_payloads_survive_until_stream_drain() {
    let hub = bcode_xray::XrayHub::default();
    let session = SessionId::new();
    let grant = XrayAccessGrant::authorized(BTreeSet::from([session]), true).unwrap();
    let observation = hub
        .observe(XrayObserveRequest {
            schema_version: 1,
            session_id: session.to_string(),
            deep_capture: true,
        })
        .unwrap();
    let id = observation.id().to_owned();
    let reference = hub
        .capture_payload(
            &bcode_xray_models::XrayCorrelation {
                session_id: session.to_string(),
                ..Default::default()
            },
            "application/json",
            b"{\"retained\":true}",
        )
        .unwrap()
        .unwrap();
    let registry = Arc::new(control::Registry::default());
    let registration = registry.register(&id, session.to_string(), 1).unwrap();
    let calls = Arc::new(Semaphore::new(1));
    let (_shutdown, stopped) = watch::channel(false);
    let response = registered_observation_response(
        observation,
        calls.try_acquire_owned().unwrap(),
        stopped,
        None,
        registration,
    );
    let first = registry.authorized(&id, &grant).unwrap();
    let second = registry.authorized(&id, &grant).unwrap();
    let (first, second) = tokio::join!(
        control::finalize_control(first),
        control::finalize_control(second)
    );
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let first = to_bytes(first.into_body(), MAX_REQUEST_BYTES)
        .await
        .unwrap();
    let second = to_bytes(second.into_body(), MAX_REQUEST_BYTES)
        .await
        .unwrap();
    assert_eq!(first, second);
    assert!(!hub.deep_capture_active(&session.to_string()));
    // No body consumption yet: cutoff is not drain or transport completion.
    assert_eq!(registry.active_count(), 1);
    let payload_request = XrayReadPayloadRequest {
        schema_version: 1,
        observation_id: id.clone(),
        session_id: session.to_string(),
        payload_id: reference.payload_id,
        offset: 0,
        length: 1024,
    };
    assert_eq!(
        hub.read_payload(&payload_request).unwrap().bytes,
        b"{\"retained\":true}"
    );
    let bytes = to_bytes(response.into_body(), MAX_FRAME_BYTES)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(text.matches("\"name\":\"finalized\"").count(), 1);
    // Fetch after remote SSE EOF, not just before draining. Cutoff does not release.
    assert_eq!(
        hub.read_payload(&payload_request).unwrap().bytes,
        b"{\"retained\":true}"
    );
    assert_eq!(
        control::release_control(registry.authorized(&id, &grant).unwrap())
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    timeout(Duration::from_secs(1), async {
        while registry.active_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(hub.read_payload(&payload_request).is_err());
    assert!(registry.authorized(&id, &grant).is_err());
}

#[tokio::test(start_paused = true)]
async fn default_subscription_stays_hot_idle_beyond_previous_lifetime_and_across_turns() {
    use futures::StreamExt;
    assert!(XrayHttpConfig::default().max_observation_duration.is_none());
    let hub = bcode_xray::XrayHub::default();
    let session = SessionId::new();
    let grant = XrayAccessGrant::authorized(BTreeSet::from([session]), false).unwrap();
    let observation = hub
        .observe(XrayObserveRequest {
            schema_version: 1,
            session_id: session.to_string(),
            deep_capture: false,
        })
        .unwrap();
    let id = observation.id().to_owned();
    let registry = Arc::new(control::Registry::default());
    let registration = registry.register(&id, session.to_string(), 1).unwrap();
    let (_shutdown, stopped) = watch::channel(false);
    let response = registered_observation_response(
        observation,
        Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
        stopped,
        XrayHttpConfig::default().max_observation_duration,
        registration,
    );
    let mut body = response.into_body().into_data_stream();
    let ready = body.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&ready).contains("\"name\":\"ready\""));
    for turn in ["first-turn", "second-turn"] {
        tokio::time::advance(Duration::from_hours(7 * 24)).await;
        tokio::task::yield_now().await;
        assert!(hub.capture_active(&session.to_string()));
        assert!(!hub.deep_capture_active(&session.to_string()));
        assert_eq!(registry.active_count(), 1);
        hub.emit(bcode_xray_models::XrayEvent {
            domain: "test".into(),
            name: "turn".into(),
            correlation: bcode_xray_models::XrayCorrelation {
                session_id: session.to_string(),
                turn_id: Some(turn.into()),
                ..Default::default()
            },
            data: serde_json::json!({}),
            sensitive: false,
        });
        timeout(Duration::from_secs(1), async {
            loop {
                let bytes = body.next().await.unwrap().unwrap();
                if String::from_utf8_lossy(&bytes).contains(turn) {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }
    let cutoff = registry.authorized(&id, &grant).unwrap();
    assert_eq!(
        control::finalize_control(cutoff).await.status(),
        StatusCode::OK
    );
    let mut finalized = false;
    while let Some(bytes) = body.next().await {
        finalized |= String::from_utf8_lossy(&bytes.unwrap()).contains("\"name\":\"finalized\"");
    }
    assert!(finalized);
    assert_eq!(
        control::release_control(registry.authorized(&id, &grant).unwrap())
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    timeout(Duration::from_secs(1), async {
        while registry.active_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!hub.capture_active(&session.to_string()));
}

#[tokio::test(start_paused = true)]
async fn finalized_lease_renews_on_payload_activity_then_expires_without_capture() {
    let registry = Arc::new(control::Registry::default());
    let session = SessionId::new();
    let grant = XrayAccessGrant::authorized(BTreeSet::from([session]), false).unwrap();
    let (mut registration, _requests, _receipt) =
        registry.register("lease", session.to_string(), 1).unwrap();
    let control = registry.authorized("lease", &grant).unwrap();
    let (_shutdown, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        registration.retain_payloads(&mut stopped).await;
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(50)).await;
    control.activity.send_replace(Instant::now());
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(50)).await;
    tokio::task::yield_now().await;
    assert_eq!(registry.active_count(), 1);
    tokio::time::advance(Duration::from_secs(11)).await;
    task.await.unwrap();
    assert_eq!(registry.active_count(), 0);
}

#[tokio::test]
async fn release_requires_cutoff_and_cannot_cancel_active_capture() {
    let registry = Arc::new(control::Registry::default());
    let session = SessionId::new();
    let grant = XrayAccessGrant::authorized(BTreeSet::from([session]), false).unwrap();
    let (_registration, _requests, _receipt) =
        registry.register("active", session.to_string(), 1).unwrap();
    let control = registry.authorized("active", &grant).unwrap();
    assert_eq!(
        control::release_control(control.clone()).await.status(),
        StatusCode::CONFLICT
    );
    assert!(!*control.release.borrow());
    assert_eq!(registry.active_count(), 1);
}

#[test]
fn registry_is_bounded_scoped_and_cleans_up_without_replay_tombstones() {
    let registry = Arc::new(control::Registry::default());
    let selected = SessionId::new();
    let foreign = XrayAccessGrant::authorized(BTreeSet::from([SessionId::new()]), true).unwrap();
    let grant = XrayAccessGrant::authorized(BTreeSet::from([selected]), false).unwrap();
    let (registration, _requests, _receipt) =
        registry.register("one", selected.to_string(), 1).unwrap();
    assert!(registry.register("two", selected.to_string(), 1).is_err());
    assert!(registry.authorized("one", &foreign).is_err());
    assert!(registry.authorized("one", &grant).is_ok());
    drop(registration);
    assert_eq!(registry.active_count(), 0);
    assert!(registry.authorized("one", &grant).is_err());
}

#[tokio::test]
async fn finalize_parser_accepts_current_client_body_and_rejects_extra_authority_fields() {
    let request = |body: &'static str| {
        Request::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let valid = request(r#"{"schema_version":1,"observation_id":"one"}"#);
    assert_eq!(
        request_json::<control::FinalizeRequest>(valid)
            .await
            .unwrap()
            .observation_id,
        "one"
    );
    let invalid = request(
        r#"{"schema_version":1,"observation_id":"one","session_id":"other","cancel":true}"#,
    );
    assert!(
        request_json::<control::FinalizeRequest>(invalid)
            .await
            .is_err()
    );
}
