//! Local, read-only HTTP/SSE adaptation of the authorized X-ray application boundary.
//!
//! No credential URLs, cookies, control routes, durable cursor, or replay. Browser clients
//! must use authenticated fetch streaming (native `EventSource` cannot supply a bearer).
//! This is sensitive diagnostics, not the ordinary public frontend event protocol.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::future::{Future, IntoFuture};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::stream;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::time::{Instant, sleep_until, timeout};

use crate::xray_application::{XrayAccessGrant, XrayApplication, XrayApplicationError};
use bcode_xray::XrayObservation;
use bcode_xray_models::{XrayObserveRequest, XrayReadPayloadRequest};

mod control;

#[cfg(test)]
mod control_tests;
#[cfg(test)]
mod socket_tests;
#[cfg(test)]
mod tests;

const MAX_REQUEST_BYTES: usize = 4096;
const MAX_FRAME_BYTES: usize = 128 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT: Duration = Duration::from_secs(15);
// Applies only after cutoff: active idle subscriptions never expire.
const FINALIZED_PAYLOAD_IDLE: Duration = Duration::from_secs(60);

/// Bounded local listener configuration. Remote binding is intentionally unsupported in v1.
#[derive(Clone, Debug)]
pub struct XrayHttpConfig {
    /// Defaults to an ephemeral IPv4 loopback port.
    pub bind: SocketAddr,
    /// Exact serialized browser origins; empty denies all Origin-bearing requests.
    /// Only HTTP loopback origins are permitted. No wildcards, `null`, or credentials.
    pub allowed_origins: BTreeSet<String>,
    /// Shared request/stream admission bound (1..=128), not a raw TCP socket cap.
    pub max_connections: usize,
    /// Optional consumer-requested cutoff, followed by engine finalization.
    /// `None` (the default) keeps subscriptions hot across idle periods and turns,
    /// with no lifetime deadline. Do not populate this from an implicit server policy.
    pub max_observation_duration: Option<Duration>,
}

impl Default for XrayHttpConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            allowed_origins: BTreeSet::new(),
            max_connections: 16,
            max_observation_duration: None,
        }
    }
}

impl XrayHttpConfig {
    fn validate(&self) -> Result<(), XrayHttpStartError> {
        if !self.bind.ip().is_loopback()
            || !(1..=128).contains(&self.max_connections)
            || self.max_observation_duration.is_some_and(|duration| {
                duration.is_zero() || Instant::now().checked_add(duration).is_none()
            })
            || self.allowed_origins.len() > 16
            || self
                .allowed_origins
                .iter()
                .any(|origin| !valid_origin(origin))
        {
            return Err(XrayHttpStartError::InvalidConfiguration);
        }
        Ok(())
    }
}

fn valid_origin(origin: &str) -> bool {
    if origin.len() > 256 {
        return false;
    }
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.origin().ascii_serialization() == origin
        && url.host_str().is_some_and(|host| {
            host == "localhost"
                || host == "[::1]"
                || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
        })
}

/// Private bearer credential bound to one authorized application scope.
///
/// Only its SHA-256 digest is retained; it is never logged or returned by this adapter.
/// Dropping/restarting the listener revokes this transport credential.
#[derive(Clone)]
pub struct XrayHttpAccess {
    digest: [u8; 32],
    grant: XrayAccessGrant,
    observations: Arc<control::Registry>,
}

impl XrayHttpAccess {
    /// Bind a private, independently generated bearer to an already-authorized scope.
    /// Caller must generate at least 256 bits of cryptographic entropy, keep the raw token
    /// private, and deliver it out of band (never in URLs, logs, config dumps, or events).
    ///
    /// # Errors
    /// Rejects short, oversized, or non-token credentials; entropy is the caller's duty.
    pub fn new(token: &str, grant: XrayAccessGrant) -> Result<Self, XrayHttpStartError> {
        if !(43..=256).contains(&token.len())
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        {
            return Err(XrayHttpStartError::InvalidCredential);
        }
        Ok(Self {
            digest: Sha256::digest(token.as_bytes()).into(),
            grant,
            observations: Arc::new(control::Registry::default()),
        })
    }

    /// Live observers, including finalized streams still draining retained payloads.
    /// Bootstrap may keep a clone and use this to distinguish unused endpoints from
    /// idle active SSE. No execution activity or completion is inferred from this count.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.observations.active_count()
    }

    fn authenticate(&self, headers: &HeaderMap) -> bool {
        let mut values = headers.get_all(header::AUTHORIZATION).iter();
        let Some(value) = values.next() else {
            return false;
        };
        if values.next().is_some() {
            return false;
        }
        let Some(token) = value.to_str().ok().and_then(|s| s.strip_prefix("Bearer ")) else {
            return false;
        };
        if !(43..=256).contains(&token.len()) {
            return false;
        }
        let supplied: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        supplied
            .iter()
            .zip(self.digest.iter())
            .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }
}

/// Fixed, secret-safe startup failure; private IO details are deliberately not surfaced.
#[derive(Debug, thiserror::Error)]
pub enum XrayHttpStartError {
    /// Invalid or non-loopback binding, origin, duration, or capacity.
    #[error("invalid local X-ray HTTP configuration")]
    InvalidConfiguration,
    /// A private bearer credential must be supplied explicitly.
    #[error("invalid private X-ray bearer credential")]
    InvalidCredential,
    /// The requested local socket could not be opened or inspected.
    #[error("X-ray HTTP listener is unavailable")]
    ListenerUnavailable,
    /// HTTP serving failed; this does not change execution outcomes.
    #[error("X-ray HTTP serving stopped unexpectedly")]
    ServeFailed,
}

struct HttpState {
    application: XrayApplication,
    access: XrayHttpAccess,
    config: XrayHttpConfig,
    authority: String,
    control_calls: Arc<Semaphore>,
    calls: Arc<Semaphore>,
    observations: Arc<control::Registry>,
    shutdown: watch::Receiver<bool>,
}

/// Bind a loopback socket without starting a daemon or spawning any task.
/// Call `local_addr()` to publish only the address, never the private bearer.
///
/// # Errors
/// Returns fixed configuration or listener errors without exposing private OS details.
pub async fn bind(config: &XrayHttpConfig) -> Result<TcpListener, XrayHttpStartError> {
    config.validate()?;
    TcpListener::bind(config.bind)
        .await
        .map_err(|_| XrayHttpStartError::ListenerUnavailable)
}

/// Serve an already-bound loopback socket until the supplied shutdown future resolves.
///
/// The caller owns task supervision and the bearer lifecycle. Shutdown finalizes observer
/// cutoffs and closes streams; it never shuts down execution or the containing daemon.
/// Slow/disconnected readers have no delivery-completion guarantee and must treat EOF as
/// incomplete unless they actually receive the engine's finalized envelope.
///
/// # Errors
/// Rejects mismatched/non-loopback sockets or configuration and reports fixed serve errors.
pub async fn serve(
    listener: TcpListener,
    application: XrayApplication,
    access: XrayHttpAccess,
    config: XrayHttpConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), XrayHttpStartError> {
    config.validate()?;
    let address = listener
        .local_addr()
        .map_err(|_| XrayHttpStartError::ListenerUnavailable)?;
    if !address.ip().is_loopback()
        || address.ip() != config.bind.ip()
        || (config.bind.port() != 0 && address.port() != config.bind.port())
    {
        return Err(XrayHttpStartError::InvalidConfiguration);
    }
    let (stop, stopped) = watch::channel(false);
    let mut graceful_stop = stopped.clone();
    let observations = Arc::clone(&access.observations);
    let state = Arc::new(HttpState {
        application,
        access,
        control_calls: Arc::new(Semaphore::new(4)),
        calls: Arc::new(Semaphore::new(config.max_connections)),
        observations: Arc::clone(&observations),
        config,
        authority: address.to_string(),
        shutdown: stopped,
    });
    let listener = BoundedListener {
        inner: listener,
        sockets: Arc::new(Semaphore::new(state.config.max_connections + 4)),
    };
    let app = Router::new()
        .route("/v1/xray/observe", post(observe))
        .route("/v1/xray/release", post(control::release))
        .route("/v1/xray/finalize", post(control::finalize))
        .route("/v1/xray/payload", post(payload))
        .fallback(|| async { public_error(StatusCode::NOT_FOUND, "xray_route_not_found") })
        .layer(middleware::from_fn_with_state(Arc::clone(&state), guard))
        .with_state(Arc::clone(&state));
    drop(observations);
    drop(state);
    // Graceful HTTP shutdown alone can wait forever on a stalled SSE reader. After
    // notifying observations, cap transport draining without making delivery claims.
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = graceful_stop.changed().await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::pin!(shutdown);
    tokio::select! {
        result = &mut server => result.map_err(|_| XrayHttpStartError::ServeFailed),
        () = &mut shutdown => {
            let _ = stop.send(true);
            timeout(Duration::from_secs(2), &mut server).await.map_or(Ok(()), |result| {
                result.map_err(|_| XrayHttpStartError::ServeFailed)
            })
        }
    }
}

// Axum owns HTTP parsing. Bound accepted sockets before it allocates per-connection
// state; idle/header trickle connections expire without affecting execution.
struct BoundedListener {
    inner: TcpListener,
    sockets: Arc<Semaphore>,
}

impl axum::serve::Listener for BoundedListener {
    type Io = BoundedSocket;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok(permit) = Arc::clone(&self.sockets).acquire_owned().await else {
                std::future::pending::<()>().await;
                continue;
            };
            match self.inner.accept().await {
                Ok((socket, address)) if address.ip().is_loopback() => {
                    return (
                        BoundedSocket {
                            inner: socket,
                            _permit: permit,
                            idle: Box::pin(tokio::time::sleep(REQUEST_TIMEOUT)),
                        },
                        address,
                    );
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

struct BoundedSocket {
    inner: tokio::net::TcpStream,
    _permit: tokio::sync::OwnedSemaphorePermit,
    idle: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl AsyncRead for BoundedSocket {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.idle.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()));
        }
        // Read activity intentionally does not extend this deadline: trickled headers
        // cannot occupy a socket forever. Successful writes/heartbeats extend it.
        std::pin::Pin::new(&mut this.inner).poll_read(cx, buffer)
    }
}

impl AsyncWrite for BoundedSocket {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.idle.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()));
        }
        let result = std::pin::Pin::new(&mut this.inner).poll_write(cx, bytes);
        if matches!(&result, std::task::Poll::Ready(Ok(written)) if *written > 0) {
            this.idle
                .as_mut()
                .reset(Instant::now() + HEARTBEAT + REQUEST_TIMEOUT);
        }
        result
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
}

fn public_error(status: StatusCode, code: &'static str) -> Response {
    (status, Json(ErrorBody { code })).into_response()
}

/// Small transport rejection; response allocation happens only at the handler boundary.
#[derive(Debug, Clone, Copy)]
struct HttpFailure {
    status: StatusCode,
    code: &'static str,
}

impl HttpFailure {
    #[cfg(test)]
    const fn status(self) -> StatusCode {
        self.status
    }
}

impl IntoResponse for HttpFailure {
    fn into_response(self) -> Response {
        public_error(self.status, self.code)
    }
}

const fn failure(status: StatusCode, code: &'static str) -> HttpFailure {
    HttpFailure { status, code }
}

const fn application_failure(error: XrayApplicationError) -> HttpFailure {
    let status = match error {
        XrayApplicationError::InvalidRequest => StatusCode::BAD_REQUEST,
        XrayApplicationError::Forbidden => StatusCode::FORBIDDEN,
        XrayApplicationError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        XrayApplicationError::NotAdmitted => StatusCode::TOO_MANY_REQUESTS,
    };
    failure(status, error.code())
}

fn application_error(error: XrayApplicationError) -> Response {
    application_failure(error).into_response()
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

fn request_guard(
    access: &XrayHttpAccess,
    config: &XrayHttpConfig,
    authority: &str,
    request: &Request,
) -> Result<Option<String>, HttpFailure> {
    let headers = request.headers();
    if request.uri().to_string().len() > 512
        || headers.len() > 64
        || headers
            .iter()
            .map(|(name, value)| name.as_str().len() + value.len())
            .sum::<usize>()
            > 16 * 1024
    {
        return Err(failure(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "xray_request_too_large",
        ));
    }
    // No queries at all: credentials, arbitrary paths and replay state cannot ride in URLs.
    if request.uri().query().is_some()
        || request.uri().scheme().is_some()
        || single_header(headers, "host") != Some(authority)
        || headers.contains_key("last-event-id")
        || headers.contains_key(header::COOKIE)
    {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "xray_invalid_transport_request",
        ));
    }
    let origin = if headers.contains_key(header::ORIGIN) {
        let origin = single_header(headers, "origin")
            .filter(|origin| config.allowed_origins.contains(*origin))
            .ok_or_else(|| failure(StatusCode::FORBIDDEN, "xray_origin_forbidden"))?;
        Some(origin.to_owned())
    } else {
        if single_header(headers, "sec-fetch-site").is_some_and(|site| site != "none") {
            return Err(failure(StatusCode::FORBIDDEN, "xray_origin_required"));
        }
        None
    };
    if request.method() != axum::http::Method::OPTIONS && !access.authenticate(headers) {
        return Err(failure(StatusCode::UNAUTHORIZED, "xray_bearer_required"));
    }
    Ok(origin)
}

async fn guard(State(state): State<Arc<HttpState>>, request: Request, next: Next) -> Response {
    let origin = match request_guard(&state.access, &state.config, &state.authority, &request) {
        Ok(origin) => origin,
        Err(response) => return secure_headers(response.into_response(), None),
    };
    let response = if request.method() == axum::http::Method::OPTIONS {
        // A browser preflight has no bearer and performs no observation or content access.
        // Only exact configured origins, read-only routes and required fetch headers qualify.
        let valid = origin.is_some()
            && matches!(
                request.uri().path(),
                "/v1/xray/observe" | "/v1/xray/payload" | "/v1/xray/finalize" | "/v1/xray/release"
            )
            && single_header(request.headers(), "access-control-request-method") == Some("POST")
            && single_header(request.headers(), "access-control-request-headers").is_some_and(
                |value| {
                    let names: BTreeSet<_> = value
                        .split(',')
                        .map(|name| name.trim().to_ascii_lowercase())
                        .collect();
                    names == BTreeSet::from(["authorization".to_owned(), "content-type".to_owned()])
                },
            );
        if valid {
            (
                StatusCode::NO_CONTENT,
                [
                    (header::ACCESS_CONTROL_ALLOW_METHODS, "POST"),
                    (
                        header::ACCESS_CONTROL_ALLOW_HEADERS,
                        "authorization, content-type",
                    ),
                ],
            )
                .into_response()
        } else {
            public_error(StatusCode::FORBIDDEN, "xray_preflight_forbidden")
        }
    } else {
        next.run(request).await
    };
    secure_headers(response, origin.as_deref())
}

fn secure_headers(mut response: Response, origin: Option<&str>) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
    );
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    if let Some(origin) = origin.and_then(|origin| HeaderValue::from_str(origin).ok()) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    response
}

async fn request_json<T: serde::de::DeserializeOwned>(request: Request) -> Result<T, HttpFailure> {
    if single_header(request.headers(), "content-type") != Some("application/json")
        || request.headers().contains_key(header::CONTENT_ENCODING)
    {
        return Err(failure(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "xray_json_required",
        ));
    }
    let bytes = timeout(
        REQUEST_TIMEOUT,
        to_bytes(request.into_body(), MAX_REQUEST_BYTES),
    )
    .await
    .map_err(|_| failure(StatusCode::REQUEST_TIMEOUT, "xray_request_timeout"))?
    .map_err(|_| failure(StatusCode::PAYLOAD_TOO_LARGE, "xray_request_too_large"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "xray_invalid_request"))
}

async fn observe(State(state): State<Arc<HttpState>>, request: Request) -> Response {
    let Ok(permit) = Arc::clone(&state.calls).try_acquire_owned() else {
        return public_error(StatusCode::TOO_MANY_REQUESTS, "xray_http_capacity");
    };
    if *state.shutdown.borrow() {
        return public_error(StatusCode::SERVICE_UNAVAILABLE, "xray_http_stopping");
    }
    let input: XrayObserveRequest = match request_json(request).await {
        Ok(input) => input,
        Err(response) => return response.into_response(),
    };
    let session_id = input.session_id.clone();
    let observation = match timeout(
        REQUEST_TIMEOUT,
        state.application.observe(&state.access.grant, input),
    )
    .await
    {
        Ok(Ok(observation)) => observation,
        Ok(Err(error)) => return application_error(error),
        Err(_) => return public_error(StatusCode::SERVICE_UNAVAILABLE, "xray_admission_timeout"),
    };
    let registration = match state.observations.register(
        observation.id(),
        session_id,
        state.config.max_connections,
    ) {
        Ok(registration) => registration,
        Err(response) => return response.into_response(),
    };
    registered_observation_response(
        observation,
        permit,
        state.shutdown.clone(),
        state.config.max_observation_duration,
        registration,
    )
}

#[cfg(test)]
fn observation_response(
    observation: XrayObservation,
    permit: tokio::sync::OwnedSemaphorePermit,
    shutdown: watch::Receiver<bool>,
    duration: Duration,
) -> Response {
    let registry = Arc::new(control::Registry::default());
    let registration = registry
        .register(observation.id(), String::new(), 1)
        .unwrap();
    registered_observation_response(observation, permit, shutdown, Some(duration), registration)
}

fn registered_observation_response(
    observation: XrayObservation,
    permit: tokio::sync::OwnedSemaphorePermit,
    shutdown: watch::Receiver<bool>,
    duration: Option<Duration>,
    registration: (
        control::Registration,
        watch::Receiver<bool>,
        watch::Sender<control::Cutoff>,
    ),
) -> Response {
    let (registration, finalize_requested, cutoff) = registration;
    let stream_state = ObservationStream {
        observation,
        _permit: permit,
        shutdown,
        registration,
        finalize_requested,
        cutoff,
        deadline: duration.and_then(|duration| Instant::now().checked_add(duration)),
        closing: false,
        finished: false,
    };
    // A bounded pump keeps capture lifetime independent of HTTP body polling. One queued
    // frame plus a five-second send budget makes stalled readers release observation state.
    let (sender, receiver) = mpsc::channel(1);
    let pump = tokio::spawn(async move {
        let mut state = stream_state;
        loop {
            let next = tokio::select! {
                biased;
                () = sender.closed() => break,
                next = next_event(&mut state) => next,
            };
            let Some(event) = next else { break };
            let terminal = matches!(&event, Ok((_, true)));
            if !state.send_event(&sender, event).await {
                break;
            }
            if terminal {
                break;
            }
        }
        let finalized = matches!(*state.cutoff.borrow(), Some(Ok(_)));
        if finalized {
            state
                .registration
                .retain_payloads(&mut state.shutdown)
                .await;
        }
    });
    // Dropping a JoinHandle detaches rather than aborts. Active disconnect is noticed by
    // sender.closed(); finalized disconnect retains only the bounded payload lease.
    let output = stream::unfold(
        (receiver, pump, false),
        |(mut receiver, pump, terminal)| async move {
            if terminal {
                return None;
            }
            receiver.recv().await.map(|event| {
                let terminal = matches!(&event, Ok((_, true)));
                (event.map(|(event, _)| event), (receiver, pump, terminal))
            })
        },
    );
    Sse::new(output)
        .keep_alive(
            KeepAlive::new()
                .interval(HEARTBEAT)
                .text("xray heartbeat; no replay"),
        )
        .into_response()
}

async fn next_event(state: &mut ObservationStream) -> Option<Result<(Event, bool), Infallible>> {
    if state.finished {
        return None;
    }
    let stopping = *state.shutdown.borrow() || *state.finalize_requested.borrow();
    if !state.closing {
        if !stopping {
            tokio::select! {
                biased;
                _ = state.shutdown.changed() => {},
                _ = state.finalize_requested.changed() => {},
                () = optional_deadline(state.deadline) => {},
                envelope = state.observation.recv() => {
                    return envelope.map(|envelope| Ok(frame(state, &envelope)));
                }
            }
        }
        state.closing = true;
        if state.establish_cutoff().is_err() {
            state.finished = true;
            return Some(Ok((transport_failure(), true)));
        }
    }
    let envelope = state.observation.recv().await;
    envelope.map(|envelope| Ok(frame(state, &envelope)))
}

fn frame(
    state: &mut ObservationStream,
    envelope: &bcode_xray_models::XrayEnvelope,
) -> (Event, bool) {
    if envelope.schema_version != 1 {
        state.finished = true;
        return (transport_failure(), true);
    }
    let event = if let Ok(json) = bounded_json(envelope) {
        Event::default().event("xray").data(json)
    } else {
        state.finished = true;
        transport_failure()
    };
    // No SSE id field: sequence is diagnostic ordering, never a replay cursor.
    let terminal =
        state.finished || (envelope.event.domain == "xray" && envelope.event.name == "finalized");
    (event, terminal)
}

struct ObservationStream {
    observation: XrayObservation,
    _permit: tokio::sync::OwnedSemaphorePermit,
    registration: control::Registration,
    finalize_requested: watch::Receiver<bool>,
    cutoff: watch::Sender<control::Cutoff>,
    shutdown: watch::Receiver<bool>,
    deadline: Option<Instant>,
    closing: bool,
    finished: bool,
}

async fn optional_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

impl ObservationStream {
    async fn send_event(
        &mut self,
        sender: &mpsc::Sender<Result<(Event, bool), Infallible>>,
        event: Result<(Event, bool), Infallible>,
    ) -> bool {
        let send = sender.send(event);
        tokio::pin!(send);
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let requested = *self.finalize_requested.borrow() || *self.shutdown.borrow();
            if *self.registration.release.borrow() {
                return false;
            }
            if !self.closing && requested {
                self.closing = true;
                if self.establish_cutoff().is_err() {
                    return false;
                }
            }
            tokio::select! {
                _ = self.registration.release.changed() => return false,
                result = &mut send => return result.is_ok(),
                () = sleep_until(deadline) => return false,
                _ = self.finalize_requested.changed(), if !self.closing => {
                    self.closing = true;
                    if self.establish_cutoff().is_err() { return false; }
                }
                _ = self.shutdown.changed(), if !self.closing => {
                    self.closing = true;
                    if self.establish_cutoff().is_err() { return false; }
                }
                () = optional_deadline(self.deadline), if !self.closing => {
                    self.closing = true;
                    if self.establish_cutoff().is_err() { return false; }
                }
            }
        }
    }

    fn establish_cutoff(&mut self) -> Result<(), ()> {
        if let Some(result) = self.cutoff.borrow().as_ref() {
            return result.as_ref().map(|_| ()).map_err(|error| *error);
        }
        let result = self.observation.finalize().map_err(|_| ());
        let success = result.is_ok();
        self.cutoff.send_replace(Some(result));
        if success { Ok(()) } else { Err(()) }
    }
}

fn transport_failure() -> Event {
    Event::default()
        .event("xray.transport_error")
        .data("{\"code\":\"xray_transport_incomplete\"}")
}

async fn payload(State(state): State<Arc<HttpState>>, request: Request) -> Response {
    let Ok(_permit) = state.control_calls.try_acquire() else {
        return public_error(StatusCode::TOO_MANY_REQUESTS, "xray_http_capacity");
    };
    if *state.shutdown.borrow() {
        return public_error(StatusCode::SERVICE_UNAVAILABLE, "xray_http_stopping");
    }
    let input: XrayReadPayloadRequest = match request_json(request).await {
        Ok(input) => input,
        Err(response) => return response.into_response(),
    };
    let control = match state
        .observations
        .authorized(&input.observation_id, &state.access.grant)
    {
        Ok(control) => control,
        Err(response) => return response.into_response(),
    };
    if control.session_id != input.session_id {
        return public_error(StatusCode::FORBIDDEN, "xray_forbidden");
    }
    match state.application.read_payload(&state.access.grant, &input) {
        Ok(range) => {
            control.activity.send_replace(Instant::now());
            match bounded_json_limit(&range, 8 * 1024 * 1024) {
                Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
                Err(()) => public_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "xray_transport_incomplete",
                ),
            }
        }
        Err(error) => application_error(error),
    }
}

// Serialize through a bounded writer, not to_vec followed by a size check. Unexpected
// engine configuration must not cause an unbounded intermediate transport allocation.
fn bounded_json(value: &impl Serialize) -> Result<String, ()> {
    bounded_json_limit(value, MAX_FRAME_BYTES)
}

fn bounded_json_limit(value: &impl Serialize, limit: usize) -> Result<String, ()> {
    struct Limited(Vec<u8>, usize);
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.1.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("X-ray transport frame limit"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Limited(Vec::new(), limit);
    serde_json::to_writer(&mut output, value).map_err(|_| ())?;
    String::from_utf8(output.0).map_err(|_| ())
}
