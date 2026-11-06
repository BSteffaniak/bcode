//! Deterministic gateway boundary tests; synthetic engine, no daemon or provider.
use super::*;

fn fixture() -> (Arc<HostState>, tokio::sync::watch::Sender<bool>) {
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let state = HostState {
        authority: "127.0.0.1:4321".into(),
        origin: "http://127.0.0.1:4321".into(),
        csp: content_security_policy("http://127.0.0.1:1234/").unwrap(),
        assets: BTreeMap::new(),
        pairing: Mutex::new(Pairing {
            required: true,
            code: "one-use-ticket".into(),
            expires: Instant::now() + PAIRING_LIFETIME,
            attempts: PAIRING_ATTEMPTS,
            bootstrap: Some(InspectorBootstrap {
                schema_version: 1,
                endpoint: "/".into(),
                bearer: "gateway-only".into(),
                session_id: "synthetic".into(),
                demo: true,
                deep_capture: true,
            }),
        }),
        gateway: Some(gateway::Gateway::new(
            backend::Backend::demo(),
            "gateway-only".into(),
            "synthetic".into(),
            true,
        )),
        stopped,
    };
    (Arc::new(state), stop)
}

fn post(path: &str, value: &serde_json::Value) -> Request {
    let value = serde_json::to_vec(value).unwrap();
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("host", "127.0.0.1:4321")
        .header("origin", "http://127.0.0.1:4321")
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-dest", "empty")
        .header("content-type", "application/json")
        .header("authorization", "Bearer gateway-only")
        .body(Body::from(value))
        .unwrap()
}
fn observe() -> serde_json::Value {
    serde_json::json!({"schema_version":1,"session_id":"synthetic","deep_capture":true})
}
async fn frame(body: &mut Body) -> String {
    use axum::body::HttpBody as _;
    let frame = std::future::poll_fn(|cx| std::pin::Pin::new(&mut *body).poll_frame(cx))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap()
}
fn envelopes(text: &str) -> Vec<bcode_xray_models::XrayEnvelope> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|json| serde_json::from_str(json).unwrap())
        .collect()
}
async fn json(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn gateway_rejects_foreign_scope_versions_headers_and_arbitrary_routes() {
    let (state, _stop) = fixture();
    for (value, expected) in [
        (
            serde_json::json!({"schema_version":2,"session_id":"synthetic","deep_capture":true}),
            StatusCode::FORBIDDEN,
        ),
        (
            serde_json::json!({"schema_version":1,"session_id":"foreign","deep_capture":true}),
            StatusCode::FORBIDDEN,
        ),
        (
            serde_json::json!({"schema_version":1,"session_id":"synthetic","deep_capture":false}),
            StatusCode::FORBIDDEN,
        ),
        (
            serde_json::json!({"schema_version":1,"session_id":"synthetic","deep_capture":true,"url":"http://other"}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(
            serve(State(state.clone()), post("/v1/xray/observe", &value))
                .await
                .status(),
            expected
        );
    }
    for (header, value) in [
        ("authorization", "Bearer raw-daemon-token"),
        ("origin", "null"),
        ("host", "localhost:4321"),
        ("cookie", "ambient=secret"),
        ("last-event-id", "replay"),
        ("sec-fetch-site", "cross-site"),
    ] {
        let mut request = post("/v1/xray/observe", &observe());
        request.headers_mut().insert(
            axum::http::HeaderName::from_static(header),
            value.parse().unwrap(),
        );
        assert_eq!(
            serve(State(state.clone()), request).await.status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        serve(
            State(state.clone()),
            post("/v1/sessions/control", &observe())
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        serve(
            State(state.clone()),
            post("/v1/xray/observe?bearer=secret", &observe())
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let mut request = post("/v1/xray/observe", &observe());
    request
        .headers_mut()
        .append("authorization", "Bearer gateway-only".parse().unwrap());
    assert_eq!(
        serve(State(state.clone()), request).await.status(),
        StatusCode::FORBIDDEN
    );
    let mut request = post("/v1/xray/observe", &observe());
    *request.body_mut() = Body::from(vec![b' '; 16 * 1024 + 1]);
    assert_eq!(
        serve(State(state), request).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn synthetic_ready_gap_finalize_payload_release_are_a_real_stream_contract() {
    let (state, _stop) = fixture();
    let response = serve(State(state.clone()), post("/v1/xray/observe", &observe())).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut body = response.into_body();
    let ready = envelopes(&frame(&mut body).await).remove(0);
    assert_eq!(
        (
            ready.event.domain.as_str(),
            ready.event.name.as_str(),
            ready.sequence
        ),
        ("xray", "ready", 1)
    );
    assert_eq!(
        serve(State(state.clone()), post("/v1/xray/observe", &observe()))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let identity = serde_json::json!({"schema_version":1,"observation_id":ready.observation_id});
    assert_eq!(
        serve(
            State(state.clone()),
            post("/v1/xray/release", &identity.clone())
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    // Both sides progress concurrently behind the ready barrier; no sleep establishes completion.
    let (receipt, bytes) = tokio::join!(
        serve(
            State(state.clone()),
            post("/v1/xray/finalize", &identity.clone())
        ),
        axum::body::to_bytes(body, 16 * 1024 * 1024),
    );
    assert_eq!(receipt.status(), StatusCode::OK);
    let receipt = json(receipt).await;
    let events = envelopes(std::str::from_utf8(&bytes.unwrap()).unwrap());
    assert!(
        events
            .iter()
            .any(|event| event.event.domain == "xray" && event.event.name == "gap")
    );
    assert_eq!(events.last().unwrap().event.name, "finalized");
    assert_eq!(events.last().unwrap().event.data, receipt);
    let payload = events
        .iter()
        .find(|event| event.event.name == "synthetic_payload")
        .unwrap();
    let reference = payload.event.payload_references().unwrap().remove(0);
    let request = serde_json::json!({"schema_version":1,"observation_id":ready.observation_id,"session_id":"synthetic","payload_id":reference.payload_id,"offset":0,"length":4096});
    let range = serve(
        State(state.clone()),
        post("/v1/xray/payload", &request.clone()),
    )
    .await;
    assert_eq!(range.status(), StatusCode::OK);
    let range = json(range).await;
    assert_eq!(range["eof"], true);
    assert_ne!(
        range["bytes"].as_array().unwrap().as_slice(),
        [] as [serde_json::Value; 0]
    );
    let mut invalid = request.clone();
    invalid["extra"] = serde_json::json!(true);
    assert_eq!(
        serve(State(state.clone()), post("/v1/xray/payload", &invalid))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        serve(State(state.clone()), post("/v1/xray/release", &identity))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        serve(State(state), post("/v1/xray/payload", &request))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn cancellation_interrupts_stream_and_releases_the_observation_slot() {
    let (state, stop) = fixture();
    let response = serve(State(state.clone()), post("/v1/xray/observe", &observe())).await;
    let mut body = response.into_body();
    let _ready = frame(&mut body).await;
    stop.send_replace(true);
    // Error EOF is explicitly incomplete, never a forged finalization envelope.
    let _result = axum::body::to_bytes(body, 16 * 1024 * 1024).await;
    tokio::time::timeout(
        Duration::from_secs(15),
        state.gateway.as_ref().unwrap().retired(),
    )
    .await
    .unwrap();
}
