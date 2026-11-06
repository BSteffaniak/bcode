//! Typed, single-Origin adapter over the public observation client. Never an HTTP proxy.

use super::backend::{Backend, Stream};
use super::{HostState, constant_time_equal, single_header};
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use bcode_xray_models::{
    XRAY_SCHEMA_VERSION, XrayFinalization, XrayObserveRequest, XrayReadPayloadRequest,
};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc, watch};

const BODY_LIMIT: usize = 16 * 1024;
const FRAME_LIMIT: usize = 2 * 1024 * 1024;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Gateway {
    client: Backend,
    capability: String,
    session: String,
    deep: bool,
    operations: Semaphore,
    observation: Mutex<Option<Observation>>,
    attach: Arc<Semaphore>,
}

struct Observation {
    id: String,
    receipt: watch::Receiver<Option<XrayFinalization>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PayloadRequest {
    schema_version: u16,
    observation_id: String,
    session_id: String,
    payload_id: String,
    offset: u64,
    length: u32,
}

impl From<PayloadRequest> for XrayReadPayloadRequest {
    fn from(value: PayloadRequest) -> Self {
        Self {
            schema_version: value.schema_version,
            observation_id: value.observation_id,
            session_id: value.session_id,
            payload_id: value.payload_id,
            offset: value.offset,
            length: value.length,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    schema_version: u16,
    observation_id: String,
}

impl Gateway {
    pub(super) fn new(client: Backend, capability: String, session: String, deep: bool) -> Self {
        Self {
            client,
            capability,
            session,
            deep,
            operations: Semaphore::new(8),
            observation: Mutex::new(None),
            attach: Arc::new(Semaphore::new(1)),
        }
    }

    #[cfg(test)]
    pub(super) async fn retired(&self) {
        let _permit = self.attach.clone().acquire_owned().await.unwrap();
    }

    fn authorized(&self, request: &Request) -> bool {
        single_header(request.headers(), "authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|value| constant_time_equal(self.capability.as_bytes(), value.as_bytes()))
            && !request.headers().contains_key("last-event-id")
    }

    fn receipt(&self, id: &str) -> Option<watch::Receiver<Option<XrayFinalization>>> {
        self.observation
            .lock()
            .ok()?
            .as_ref()
            .filter(|observation| observation.id == id)
            .map(|observation| observation.receipt.clone())
    }

    fn install(
        &self,
        id: String,
        receipt: watch::Receiver<Option<XrayFinalization>>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> bool {
        let Ok(mut observation) = self.observation.lock() else {
            return false;
        };
        *observation = Some(Observation {
            id,
            receipt,
            _permit: permit,
        });
        true
    }

    fn forget(&self, id: &str) {
        if let Ok(mut observation) = self.observation.lock()
            && observation.as_ref().is_some_and(|value| value.id == id)
        {
            *observation = None;
        }
    }
}

pub(super) async fn forward(state: Arc<HostState>, request: Request) -> Response {
    let Some(gateway) = state.gateway.as_ref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !gateway.authorized(&request) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if request.method() != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = request.uri().path().to_owned();
    if !matches!(
        path.as_str(),
        "/v1/xray/observe" | "/v1/xray/finalize" | "/v1/xray/release" | "/v1/xray/payload"
    ) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if single_header(request.headers(), "content-type") != Some("application/json") {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let Ok(_operation) = gateway.operations.try_acquire() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let Ok(Ok(bytes)) =
        tokio::time::timeout(OPERATION_TIMEOUT, to_bytes(request.into_body(), BODY_LIMIT)).await
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match path.as_str() {
        "/v1/xray/observe" => observe_response(state.clone(), gateway, &bytes).await,
        "/v1/xray/payload" => {
            let Ok(request) = serde_json::from_slice::<PayloadRequest>(&bytes) else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let request = XrayReadPayloadRequest::from(request);
            if request.schema_version != XRAY_SCHEMA_VERSION
                || request.session_id != gateway.session
                || gateway.receipt(&request.observation_id).is_none()
                || request.length == 0
                || request.length > 1024 * 1024
            {
                return StatusCode::FORBIDDEN.into_response();
            }
            match tokio::time::timeout(OPERATION_TIMEOUT, gateway.client.read_payload(&request))
                .await
            {
                Ok(Ok(range)) => axum::Json(range).into_response(),
                _ => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        _ => {
            let Ok(request) = serde_json::from_slice::<Control>(&bytes) else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            if request.schema_version != XRAY_SCHEMA_VERSION || request.observation_id.len() > 4096
            {
                return StatusCode::BAD_REQUEST.into_response();
            }
            let Some(receipt) = gateway.receipt(&request.observation_id) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            if path == "/v1/xray/release" {
                if receipt.borrow().is_none() {
                    return StatusCode::CONFLICT.into_response();
                }
                return match gateway.client.release(&request.observation_id).await {
                    Ok(()) => {
                        gateway.forget(&request.observation_id);
                        StatusCode::NO_CONTENT.into_response()
                    }
                    Err(()) => StatusCode::BAD_GATEWAY.into_response(),
                };
            }
            if gateway
                .client
                .finalize(&request.observation_id)
                .await
                .is_err()
            {
                return StatusCode::BAD_GATEWAY.into_response();
            }
            finalization_response(receipt).await
        }
    }
}

async fn observe_response(state: Arc<HostState>, gateway: &Gateway, bytes: &[u8]) -> Response {
    let Ok(request) = serde_json::from_slice::<XrayObserveRequest>(bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if request.schema_version != XRAY_SCHEMA_VERSION
        || request.session_id != gateway.session
        || request.deep_capture != gateway.deep
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(permit) = gateway.attach.clone().try_acquire_owned() else {
        return StatusCode::CONFLICT.into_response();
    };
    let Ok(stream) = gateway.client.observe(request).await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let id = stream.observation_id().to_owned();
    let (receipt, receiver) = watch::channel(None);
    if !gateway.install(id.clone(), receiver, permit) {
        let _ = gateway.client.finalize(&id).await;
        let _ = gateway.client.release(&id).await;
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let (sender, receiver) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    tokio::spawn(pump(state.clone(), stream, id, receipt, sender));
    let body = Body::from_stream(futures_util::stream::unfold(
        receiver,
        |mut receiver| async move { receiver.recv().await.map(|value| (value, receiver)) },
    ));
    ([("content-type", "text/event-stream")], body).into_response()
}

async fn finalization_response(mut receipt: watch::Receiver<Option<XrayFinalization>>) -> Response {
    let result = tokio::time::timeout(OPERATION_TIMEOUT, async {
        loop {
            let current = receipt.borrow().clone();
            if let Some(value) = current {
                return Some(value);
            }
            if receipt.changed().await.is_err() {
                return None;
            }
        }
    })
    .await;
    match result {
        Ok(Some(value)) => axum::Json(value).into_response(),
        _ => StatusCode::BAD_GATEWAY.into_response(),
    }
}

async fn pump(
    state: Arc<HostState>,
    mut stream: Stream,
    id: String,
    receipt: watch::Sender<Option<XrayFinalization>>,
    sender: mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    let mut stopped = state.stopped.clone();
    let mut finalized = false;
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(10),
        Duration::from_secs(10),
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let event = tokio::select! {
            biased;
            changed = stopped.changed() => { let _ = changed; break; },
            () = sender.closed() => break,
            _ = heartbeat.tick() => {
                if !matches!(tokio::time::timeout(SEND_TIMEOUT, sender.send(Ok(Bytes::from_static(b": heartbeat\n\n")))).await, Ok(Ok(()))) { break; }
                continue;
            },
            event = stream.recv() => event,
        };
        let Ok(Some(envelope)) = event else {
            break;
        };
        let Ok(json) = serde_json::to_string(&envelope) else {
            break;
        };
        if json.len() > FRAME_LIMIT {
            break;
        }
        let frame = Bytes::from(format!("event: xray\ndata: {json}\n\n"));
        if !matches!(
            tokio::time::timeout(SEND_TIMEOUT, sender.send(Ok(frame))).await,
            Ok(Ok(()))
        ) {
            break;
        }
        if let Some(value) = stream.finalization() {
            receipt.send_replace(Some(value.clone()));
            finalized = true;
            break;
        }
    }
    if finalized {
        drop(sender);
        // Retained payload authority is transient, even if the browser never releases it.
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(60)) => {},
            _result = stopped.changed() => {},
            () = receipt.closed() => return,
        }
        if let Some(gateway) = &state.gateway {
            let _ = gateway.client.release(&id).await;
            gateway.forget(&id);
        }
    } else {
        // Never make a truncated stream look like a clean finalized EOF.
        let _ = sender.try_send(Err(std::io::Error::other("xray stream interrupted")));
        if let Some(gateway) = &state.gateway {
            let _ = gateway.client.finalize(&id).await;
            let _ = gateway.client.release(&id).await;
            gateway.forget(&id);
        }
    }
}
