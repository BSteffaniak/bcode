//! Thin HTTP transport over a long-lived invocation's normalized host bridge.
use crate::{
    config::ServeRequest,
    leases::{Lease, Leases},
    oidc::Verifier,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use bcode_model::host_operation as wire;
use bcode_plugin_sdk::{
    ServiceBridge, ServiceBridgeRequest, ServiceBridgeResponse, ServiceCancellation,
};
use bcode_tool::{ToolInvocationServiceRequest, ToolInvocationServiceResolution};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, Semaphore};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

type HttpError = (StatusCode, Json<ErrorBody>);
#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
}
const fn error(status: StatusCode, code: &'static str) -> HttpError {
    (status, Json(ErrorBody { code }))
}

fn bearer(headers: &HeaderMap) -> Result<&str, HttpError> {
    let mut values = headers.get_all(axum::http::header::AUTHORIZATION).iter();
    let header = values
        .next()
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "bearer_required"))?;
    if values.next().is_some() {
        return Err(error(StatusCode::UNAUTHORIZED, "invalid_bearer"));
    }
    header
        .to_str()
        .ok()
        .and_then(|header| header.strip_prefix("Bearer "))
        .filter(|token| {
            !token.is_empty()
                && token.len() <= 16384
                && !token.bytes().any(|byte| byte.is_ascii_whitespace())
        })
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "invalid_bearer"))
}

struct AppState {
    verifier: Verifier,
    leases: Mutex<Leases>,
    ttl: u64,
    bridge: ServiceBridge,
    invocation_id: String,
    calls: Arc<Semaphore>,
    identities: Arc<Semaphore>,
}

impl AppState {
    async fn lease(&self, headers: &HeaderMap) -> Result<Lease, HttpError> {
        let token = bearer(headers)?;
        let leases = self.leases.lock().await;
        let lease = leases.resolve(token, now());
        drop(leases);
        lease.ok_or_else(|| error(StatusCode::UNAUTHORIZED, "lease_unavailable"))
    }

    async fn host<Q: Serialize + Sync, R: DeserializeOwned + Send + 'static>(
        &self,
        operation: &'static str,
        input: &Q,
    ) -> Result<R, HttpError> {
        let permit = Arc::clone(&self.calls)
            .try_acquire_owned()
            .map_err(|_| error(StatusCode::TOO_MANY_REQUESTS, "gateway_busy"))?;
        let bridge = self.bridge.clone();
        let request = ToolInvocationServiceRequest {
            invocation_id: self.invocation_id.clone(),
            request_id: operation.to_owned(),
            route_id: None,
            interface_id: wire::INTERFACE_ID.to_owned(),
            operation: operation.to_owned(),
            payload: serde_json::to_value(input)
                .map_err(|_| error(StatusCode::BAD_REQUEST, "invalid_request"))?,
        };
        let response = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            bridge.request(&ServiceBridgeRequest::InvokeService(request))
        })
        .await
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "host_unavailable"))?
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "host_unavailable"))?;
        match response {
            ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
                payload,
            }) => serde_json::from_value(payload)
                .map_err(|_| error(StatusCode::BAD_GATEWAY, "invalid_host_response")),
            ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Failed {
                code,
                ..
            }) => {
                let (status, public) = match code.as_str() {
                    "request_conflict" | "scope_conflict" => {
                        (StatusCode::CONFLICT, "request_conflict")
                    }
                    "cursor_gap" => (StatusCode::GONE, "cursor_gap"),
                    "turn_unavailable" => (StatusCode::NOT_FOUND, "turn_unavailable"),
                    "capacity_exhausted" => (StatusCode::TOO_MANY_REQUESTS, "capacity_exhausted"),
                    "invalid_request" => (StatusCode::BAD_REQUEST, "invalid_request"),
                    "cleanup_pending" => (StatusCode::CONFLICT, "cleanup_pending"),
                    _ => (StatusCode::SERVICE_UNAVAILABLE, "model_host_failed"),
                };
                Err(error(status, public))
            }
            _ => Err(error(StatusCode::SERVICE_UNAVAILABLE, "host_unavailable")),
        }
    }
}

#[derive(Serialize)]
struct LeaseResponse {
    lease: String,
    expires_at: u64,
}

async fn exchange(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<LeaseResponse>, HttpError> {
    let _permit = Arc::clone(&state.identities)
        .try_acquire_owned()
        .map_err(|_| error(StatusCode::TOO_MANY_REQUESTS, "identity_busy"))?;
    let expiry = state
        .verifier
        .verify(bearer(&headers)?, now())
        .await
        .map_err(|_| error(StatusCode::UNAUTHORIZED, "identity_not_allowed"))?;
    let time = now();
    let expires_at = expiry.min(time.saturating_add(state.ttl));
    let mut leases = state.leases.lock().await;
    let issued = leases.issue(time, expires_at);
    drop(leases);
    let (token, lease) =
        issued.map_err(|_| error(StatusCode::TOO_MANY_REQUESTS, "lease_capacity_exhausted"))?;
    Ok(Json(LeaseResponse {
        lease: token,
        expires_at: lease.expires_at,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartBody {
    request_id: String,
    #[serde(deserialize_with = "wire::deserialize_public_messages")]
    messages: Vec<bcode_model::ModelMessage>,
    #[serde(default, deserialize_with = "wire::deserialize_public_tools")]
    tools: Vec<bcode_model::ToolDefinition>,
    #[serde(default, deserialize_with = "wire::deserialize_public_tool_policy")]
    tool_call_policy: bcode_model::ToolCallRequestPolicy,
    #[serde(default)]
    tool_schema_mode: Option<bcode_model::ToolSchemaMode>,
    max_output_tokens: Option<u32>,
}

async fn start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<StartBody>,
) -> Result<Json<wire::StartResponse>, HttpError> {
    let lease = state.lease(&headers).await?;
    state
        .host(
            wire::OP_START,
            &wire::StartRequest {
                scope_id: lease.scope,
                expires_at: lease.expires_at,
                request_id: body.request_id,
                messages: body.messages,
                tools: body.tools,
                tool_call_policy: body.tool_call_policy,
                tool_schema_mode: body.tool_schema_mode,
                max_output_tokens: body.max_output_tokens,
            },
        )
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PollBody {
    after_sequence: u64,
}

async fn poll(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PollBody>,
) -> Result<Json<wire::PollResponse>, HttpError> {
    let lease = state.lease(&headers).await?;
    state
        .host(
            wire::OP_POLL,
            &wire::PollRequest {
                scope_id: lease.scope,
                turn_id: id,
                after_sequence: body.after_sequence,
            },
        )
        .await
        .map(Json)
}

async fn cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<bcode_model::AckResponse>, HttpError> {
    let lease = state.lease(&headers).await?;
    state
        .host(
            wire::OP_CANCEL,
            &wire::TurnRequest {
                scope_id: lease.scope,
                turn_id: id,
            },
        )
        .await
        .map(Json)
}

async fn finish(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<bcode_model::AckResponse>, HttpError> {
    let lease = state.lease(&headers).await?;
    state
        .host(
            wire::OP_FINISH,
            &wire::TurnRequest {
                scope_id: lease.scope,
                turn_id: id,
            },
        )
        .await
        .map(Json)
}

pub async fn serve(
    config: ServeRequest,
    bridge: ServiceBridge,
    cancellation: ServiceCancellation,
) -> Result<(), &'static str> {
    config.validate()?;
    if !bridge.is_available() {
        return Err("model host bridge is unavailable");
    }
    let verifier = Verifier::new(config.audience, config.allow)?;
    let state = Arc::new(AppState {
        verifier,
        leases: Mutex::new(Leases::new(config.max_leases)),
        ttl: config.lease_seconds,
        bridge,
        invocation_id: config.invocation_id,
        calls: Arc::new(Semaphore::new(32)),
        identities: Arc::new(Semaphore::new(8)),
    });
    let router = Router::new()
        .route("/v1/leases", post(exchange))
        .route("/v1/turns", post(start))
        .route("/v1/turns/{id}/poll", post(poll))
        .route("/v1/turns/{id}/cancel", post(cancel))
        .route("/v1/turns/{id}/finish", post(finish))
        .layer(DefaultBodyLimit::max(196_608))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|_| "broker bind failed")?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            while !cancellation.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| "broker listener failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gateway_and_identity_futures_are_send() {
        fn require_send(_: impl Send) {}
        let state = AppState {
            verifier: Verifier::new("aud".into(), Vec::new()).unwrap(),
            leases: Mutex::new(Leases::new(1)),
            ttl: 60,
            bridge: ServiceBridge::default(),
            invocation_id: "serve".into(),
            calls: Arc::new(Semaphore::new(1)),
            identities: Arc::new(Semaphore::new(1)),
        };
        let headers = HeaderMap::new();
        let request = wire::TurnRequest {
            scope_id: "scope".into(),
            turn_id: "turn".into(),
        };
        require_send(state.lease(&headers));
        require_send(state.host::<_, bcode_model::AckResponse>(wire::OP_CANCEL, &request));
        require_send(state.verifier.verify("invalid", 100));
    }

    #[test]
    fn rejects_credential_injection_and_duplicate_authorization() {
        assert!(
            serde_json::from_str::<StartBody>(
                r#"{"request_id":"r","messages":[],"provider_context":{}}"#
            )
            .is_err()
        );
        let mut headers = HeaderMap::new();
        headers.append("authorization", "Bearer token".parse().unwrap());
        headers.append("authorization", "Bearer other".parse().unwrap());
        assert!(bearer(&headers).is_err());
    }
}
