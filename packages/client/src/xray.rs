//! Session-scoped X-ray over the public local HTTP/SSE protocol.
//!
//! This client deliberately does not discover private daemon sockets or start a daemon.
//! Callers supply a trusted endpoint and a bearer token obtained through local discovery.
//! Streams are transient: EOF without `xray.finalized` is an error, never a resume cursor.

use bcode_xray_models::{
    XRAY_SCHEMA_VERSION, XrayCapabilities, XrayEnvelope, XrayFinalization, XrayGap,
    XrayObserveRequest,
};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use sha2::{Digest as _, Sha256};
use std::collections::VecDeque;
use std::time::Duration;

const MAX_SSE_FRAME_BYTES: usize = 2 * 1024 * 1024;
const HTTP_OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CONTROL_RESPONSE_BYTES: usize = 64 * 1024;

/// Matching-application bootstrap capability and its retained revocation connection.
/// Dropped/aborted callers rely on server-side expiry; ordinary exits must call `close`.
pub struct XrayEndpointLease {
    pub(crate) info: bcode_ipc::XrayEndpointInfo,
    pub(crate) connection: crate::ClientConnection,
}

impl XrayEndpointLease {
    /// Exact one-session endpoint and private credential file supplied by the daemon.
    #[must_use]
    pub const fn info(&self) -> &bcode_ipc::XrayEndpointInfo {
        &self.info
    }

    /// Revoke the listener and its server-owned credential without cancelling session work.
    ///
    /// # Errors
    /// Reports rejected or ambiguous revocation. Never deletes the credential path locally.
    pub async fn close(mut self) -> Result<(), crate::ClientError> {
        self.connection
            .close_xray_endpoint(&self.info.endpoint_id)
            .await
    }
}

/// Secret-safe transport or protocol failure. Server bodies and request details are excluded.
#[derive(Debug, thiserror::Error)]
pub enum XrayClientError {
    /// Endpoint was not an explicit loopback HTTP origin.
    #[error(
        "X-ray endpoint must be an explicit loopback HTTP origin without credentials, path, query or fragment"
    )]
    InvalidEndpoint,
    /// Authentication material was malformed.
    #[error("invalid X-ray bearer credential")]
    InvalidCredential,
    /// No server response can be trusted as a successful operation.
    #[error("X-ray HTTP transport failed; observation may be incomplete")]
    Transport,
    /// A server rejected a request. Response bodies are never included.
    #[error("X-ray HTTP request rejected (status {0})")]
    Http(u16),
    /// Unknown, oversized, inconsistent or malformed protocol data.
    #[error("invalid or unsupported X-ray stream")]
    Protocol,
    /// Transport ended without the observation delivery cutoff.
    #[error("X-ray stream ended before finalization; evidence is incomplete")]
    Incomplete,
}

/// Authenticated public HTTP client. Debug output intentionally omits configuration.
#[derive(Clone)]
pub struct XrayClient {
    http: reqwest::Client,
    origin: reqwest::Url,
    authorization: HeaderValue,
}

impl std::fmt::Debug for XrayClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XrayClient").finish_non_exhaustive()
    }
}

impl XrayClient {
    /// Construct a client from explicitly trusted local configuration.
    ///
    /// Redirects and ambient proxies are disabled. Tokens are sent only as sensitive
    /// Authorization headers, never URL parameters. This does not start a daemon.
    ///
    /// # Errors
    /// Rejects non-loopback origins, malformed credentials and HTTP client setup failures.
    pub fn new(endpoint: &str, token: &str) -> Result<Self, XrayClientError> {
        let origin = validate_origin(endpoint)?;
        if token.is_empty() || token.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err(XrayClientError::InvalidCredential);
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| XrayClientError::InvalidCredential)?;
        authorization.set_sensitive(true);
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| XrayClientError::Transport)?;
        Ok(Self {
            http,
            origin,
            authorization,
        })
    }

    /// Attach and consume the ready barrier before returning. The ready envelope is
    /// retained and returned by the first `recv`; callers can publish it before submitting work.
    ///
    /// # Errors
    /// Returns errors for failed authorization, unsupported capabilities or missing readiness.
    pub async fn observe(
        &self,
        request: XrayObserveRequest,
    ) -> Result<XrayStream, XrayClientError> {
        if request.schema_version != XRAY_SCHEMA_VERSION || request.session_id.is_empty() {
            return Err(XrayClientError::Protocol);
        }
        let response = tokio::time::timeout(
            Duration::from_secs(15),
            self.http
                .post(self.url("v1/xray/observe")?)
                .header(AUTHORIZATION, self.authorization.clone())
                .header(ACCEPT, "text/event-stream")
                .json(&request)
                .send(),
        )
        .await
        .map_err(|_| XrayClientError::Transport)?
        .map_err(|_| XrayClientError::Transport)?;
        check_status(&response)?;
        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| value.split(';').next() != Some("text/event-stream"))
        {
            return Err(XrayClientError::Protocol);
        }
        let mut stream = XrayStream {
            response,
            decoder: SseDecoder::default(),
            pending: VecDeque::new(),
            session_id: request.session_id,
            observation_id: None,
            last_sequence: 0,
            finalized: false,
            finalization: None,
        };
        let ready = tokio::time::timeout(Duration::from_secs(15), stream.read_next())
            .await
            .map_err(|_| XrayClientError::Incomplete)??;
        if ready.event.domain != "xray" || ready.event.name != "ready" || ready.sequence != 1 {
            return Err(XrayClientError::Protocol);
        }
        let capabilities: XrayCapabilities = serde_json::from_value(ready.event.data.clone())
            .map_err(|_| XrayClientError::Protocol)?;
        if capabilities.schema_version != XRAY_SCHEMA_VERSION
            || capabilities.deep_capture != request.deep_capture
            || capabilities.durable_replay
        {
            return Err(XrayClientError::Protocol);
        }
        stream.pending.push_back(ready);
        Ok(stream)
    }

    /// Request a delivery cutoff, without cancelling or otherwise steering session work.
    /// Continue draining the SSE stream to its finalization envelope afterward. Fetch
    /// referenced payloads before `release`; finalization alone does not release retention.
    /// This operation has a 15-second request-and-body deadline, unlike live SSE reads.
    ///
    /// # Errors
    /// Returns a transport, deadline or HTTP error when cutoff acceptance is not known.
    /// Readiness and this request are bounded, but idle live `XrayStream::recv` is not.
    pub async fn finalize(&self, observation_id: &str) -> Result<(), XrayClientError> {
        self.observation_control("v1/xray/finalize", observation_id)
            .await
    }

    /// Release a finalized observation's retained payloads after consumer export finishes.
    /// The server returns 204 after engine ownership retires; an expired identity is an
    /// error, not a replay or confirmed-release receipt. Never cancels session execution.
    ///
    /// # Errors
    /// Rejects missing/unknown identities, release before cutoff, server errors and
    /// requests or bodies exceeding the 15-second intrinsic operation deadline.
    pub async fn release(&self, observation_id: &str) -> Result<(), XrayClientError> {
        self.observation_control("v1/xray/release", observation_id)
            .await
    }

    fn bounded_post(&self, path: &str) -> Result<reqwest::RequestBuilder, XrayClientError> {
        Ok(self
            .http
            .post(self.url(path)?)
            .timeout(HTTP_OPERATION_TIMEOUT)
            .header(AUTHORIZATION, self.authorization.clone()))
    }

    async fn observation_control(
        &self,
        path: &str,
        observation_id: &str,
    ) -> Result<(), XrayClientError> {
        if observation_id.is_empty() || observation_id.len() > 128 {
            return Err(XrayClientError::Protocol);
        }
        // RequestBuilder::timeout covers headers and the entire consumed response body.
        // It is per request only: the HTTP client itself has no live-stream deadline.
        let mut response = self
            .bounded_post(path)?
            .json(&serde_json::json!({
                "schema_version": XRAY_SCHEMA_VERSION,
                "observation_id": observation_id,
            }))
            .send()
            .await
            .map_err(|_| XrayClientError::Transport)?;
        check_status(&response)?;
        let mut received = 0_usize;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| XrayClientError::Transport)?
        {
            received = received.saturating_add(chunk.len());
            if received > MAX_CONTROL_RESPONSE_BYTES {
                return Err(XrayClientError::Protocol);
            }
        }
        Ok(())
    }

    /// Read an explicitly retained sensitive payload range through the public protocol.
    /// This never falls back to session files or reconstructs unavailable captures.
    /// A 15-second request deadline includes response headers and the complete body.
    /// Finalized payloads remain readable only until explicit release or server expiry.
    ///
    /// # Errors
    /// Rejects failed requests and response identity/range/version disagreement.
    pub async fn read_payload(
        &self,
        request: &bcode_xray_models::XrayReadPayloadRequest,
    ) -> Result<bcode_xray_models::XrayPayloadRange, XrayClientError> {
        if request.schema_version != XRAY_SCHEMA_VERSION
            || request.length == 0
            || request.length > 1024 * 1024
        {
            return Err(XrayClientError::Protocol);
        }
        let mut response = self
            .bounded_post("v1/xray/payload")?
            .json(request)
            .send()
            .await
            .map_err(|_| XrayClientError::Transport)?;
        check_status(&response)?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| XrayClientError::Transport)?
        {
            if bytes.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
                return Err(XrayClientError::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        let range: bcode_xray_models::XrayPayloadRange =
            serde_json::from_slice(&bytes).map_err(|_| XrayClientError::Protocol)?;
        let end = range
            .offset
            .checked_add(range.bytes.len() as u64)
            .ok_or(XrayClientError::Protocol)?;
        if range.schema_version != XRAY_SCHEMA_VERSION
            || range.payload.schema_version != XRAY_SCHEMA_VERSION
            || range.observation_id != request.observation_id
            || range.payload.session_id != request.session_id
            || range.payload.payload_id != request.payload_id
            || range.offset != request.offset
            || range.bytes.len() > request.length as usize
            || end > range.payload.byte_len
            || range.eof != (end == range.payload.byte_len)
            || (range.bytes.is_empty() && !range.eof)
        {
            return Err(XrayClientError::Protocol);
        }
        Ok(range)
    }

    /// Copy generic typed payload references carried in event data into a recording sink.
    /// Call while observation retention is live, including after the final frame but before
    /// `release`. Successful HTTP payload reads renew the server's bounded retention lease.
    /// Candidate objects containing `payload_id` or digest/length coordinates must be
    /// exactly a complete v1 `XrayPayloadRef`; malformed/future references fail closed.
    /// Traversal, reference
    /// count, payload size, ranges and per-payload fetch time are bounded. The client
    /// checks full SHA-256 and length; sink `store_payload` checks them again before
    /// claiming local retention.
    ///
    /// # Errors
    /// Fails for malformed/foreign references, missing bytes, changed metadata, bounds,
    /// transport failure, digest mismatch or recording failure. Expected references are
    /// registered before reads so a failed export cannot become a complete recording.
    pub async fn record_payloads(
        &self,
        envelope: &XrayEnvelope,
        sink: &mut dyn bcode_xray::recording::RecordingSink,
        range_bytes: u32,
    ) -> Result<(), XrayClientError> {
        let result = self
            .record_payloads_inner(envelope, sink, range_bytes)
            .await;
        if result.is_err() {
            // Even an unknown reference shape must not permit a finalized complete bundle.
            let _ = sink.interrupt();
        }
        result
    }

    async fn record_payloads_inner(
        &self,
        envelope: &XrayEnvelope,
        sink: &mut dyn bcode_xray::recording::RecordingSink,
        range_bytes: u32,
    ) -> Result<(), XrayClientError> {
        let references = payload_references(&envelope.event.data)?;
        for reference in &references {
            if reference.schema_version != XRAY_SCHEMA_VERSION
                || reference.session_id != envelope.event.correlation.session_id
            {
                return Err(XrayClientError::Protocol);
            }
            sink.expect_payload(reference)
                .map_err(|_| XrayClientError::Protocol)?;
        }
        for reference in references {
            let bytes = tokio::time::timeout(
                Duration::from_secs(15),
                self.read_complete_payload(&envelope.observation_id, &reference, range_bytes),
            )
            .await
            .map_err(|_| XrayClientError::Incomplete)??;
            sink.store_payload(&reference, &bytes)
                .map_err(|_| XrayClientError::Protocol)?;
        }
        Ok(())
    }

    async fn read_complete_payload(
        &self,
        observation_id: &str,
        reference: &bcode_xray_models::XrayPayloadRef,
        range_bytes: u32,
    ) -> Result<Vec<u8>, XrayClientError> {
        if reference.byte_len > 1024 * 1024 || range_bytes == 0 || range_bytes > 64 * 1024 {
            return Err(XrayClientError::Protocol);
        }
        let range_bytes = range_bytes.min(16 * 1024);
        let mut bytes = Vec::new();
        loop {
            let range = self
                .read_payload(&bcode_xray_models::XrayReadPayloadRequest {
                    schema_version: XRAY_SCHEMA_VERSION,
                    observation_id: observation_id.to_owned(),
                    session_id: reference.session_id.clone(),
                    payload_id: reference.payload_id.clone(),
                    offset: bytes.len() as u64,
                    length: range_bytes,
                })
                .await?;
            if &range.payload != reference {
                return Err(XrayClientError::Protocol);
            }
            bytes.extend_from_slice(&range.bytes);
            if range.eof {
                validate_payload_bytes(reference, &bytes)?;
                return Ok(bytes);
            }
        }
    }

    fn url(&self, path: &str) -> Result<reqwest::Url, XrayClientError> {
        self.origin
            .join(path)
            .map_err(|_| XrayClientError::InvalidEndpoint)
    }
}

fn validate_payload_bytes(
    reference: &bcode_xray_models::XrayPayloadRef,
    bytes: &[u8],
) -> Result<(), XrayClientError> {
    if bytes.len() as u64 != reference.byte_len
        || hex::encode(Sha256::digest(bytes)) != reference.sha256
    {
        return Err(XrayClientError::Protocol);
    }
    Ok(())
}

fn payload_references(
    data: &serde_json::Value,
) -> Result<Vec<bcode_xray_models::XrayPayloadRef>, XrayClientError> {
    let mut pending = vec![(data, 0_u8, false)];
    let mut visited = 0_usize;
    let mut references = std::collections::BTreeMap::new();
    while let Some((value, depth, expected_reference)) = pending.pop() {
        visited += 1;
        if expected_reference
            && !value.is_null()
            && !value.is_array()
            && !value
                .as_object()
                .is_some_and(|object| object.contains_key("payload_id"))
        {
            return Err(XrayClientError::Protocol);
        }
        if visited > 4096 || depth > 32 {
            return Err(XrayClientError::Protocol);
        }
        match value {
            serde_json::Value::Object(object)
                if object.contains_key("payload_id")
                    || (object.contains_key("sha256") && object.contains_key("byte_len")) =>
            {
                let fields = [
                    "schema_version",
                    "payload_id",
                    "session_id",
                    "content_type",
                    "byte_len",
                    "sha256",
                ];
                if object.len() != fields.len()
                    || object.keys().any(|key| !fields.contains(&key.as_str()))
                {
                    return Err(XrayClientError::Protocol);
                }
                let reference: bcode_xray_models::XrayPayloadRef =
                    serde_json::from_value(value.clone()).map_err(|_| XrayClientError::Protocol)?;
                if reference.schema_version != XRAY_SCHEMA_VERSION
                    || reference.payload_id.is_empty()
                    || reference.sha256.len() != 64
                    || !reference
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                {
                    return Err(XrayClientError::Protocol);
                }
                if references
                    .get(&reference.payload_id)
                    .is_some_and(|existing| existing != &reference)
                {
                    return Err(XrayClientError::Protocol);
                }
                references.insert(reference.payload_id.clone(), reference);
                if references.len() > 64 {
                    return Err(XrayClientError::Protocol);
                }
            }
            serde_json::Value::Object(object) => {
                if pending.len().saturating_add(object.len()) > 4096 {
                    return Err(XrayClientError::Protocol);
                }
                pending.extend(object.iter().map(|(key, value)| {
                    (
                        value,
                        depth + 1,
                        matches!(
                            key.as_str(),
                            "payload" | "payloads" | "payload_ref" | "payload_refs"
                        ),
                    )
                }));
            }
            serde_json::Value::Array(array) => {
                if pending.len().saturating_add(array.len()) > 4096 {
                    return Err(XrayClientError::Protocol);
                }
                pending.extend(
                    array
                        .iter()
                        .map(|value| (value, depth + 1, expected_reference)),
                );
            }
            _ => {}
        }
    }
    Ok(references.into_values().collect())
}

fn check_status(response: &reqwest::Response) -> Result<(), XrayClientError> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(XrayClientError::Http(response.status().as_u16()))
    }
}

fn validate_origin(endpoint: &str) -> Result<reqwest::Url, XrayClientError> {
    let origin = reqwest::Url::parse(endpoint).map_err(|_| XrayClientError::InvalidEndpoint)?;
    if origin.scheme() != "http"
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.query().is_some()
        || origin.fragment().is_some()
        || origin.path() != "/"
        || !origin.host_str().is_some_and(|host| {
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        })
    {
        return Err(XrayClientError::InvalidEndpoint);
    }
    Ok(origin)
}

/// One transient observation. Dropping this object closes observation transport only.
///
/// After finalization the server retains payloads for bounded export; call
/// `XrayClient::release` after exporting references, even if local recording fails.
/// Finalized transport EOF or Drop is not a payload-release acknowledgment.
pub struct XrayStream {
    response: reqwest::Response,
    decoder: SseDecoder,
    pending: VecDeque<XrayEnvelope>,
    session_id: String,
    observation_id: Option<String>,
    last_sequence: u64,
    finalized: bool,
    finalization: Option<XrayFinalization>,
}

impl XrayStream {
    /// Exact ready-barrier identity; available immediately after `observe`.
    #[must_use]
    pub fn observation_id(&self) -> &str {
        self.observation_id.as_deref().unwrap_or_default()
    }

    /// Delivery-cutoff receipt, available only after consuming the final envelope.
    #[must_use]
    pub const fn finalization(&self) -> Option<&XrayFinalization> {
        self.finalization.as_ref()
    }

    /// Receive an ordered envelope. `None` means a finalization envelope was already delivered.
    ///
    /// # Errors
    /// Rejects unknown versions, identity changes, unexplained sequence loss and abrupt EOF.
    pub async fn recv(&mut self) -> Result<Option<XrayEnvelope>, XrayClientError> {
        if let Some(envelope) = self.pending.pop_front() {
            return Ok(Some(envelope));
        }
        if self.finalized {
            return Ok(None);
        }
        self.read_next().await.map(Some)
    }

    async fn read_next(&mut self) -> Result<XrayEnvelope, XrayClientError> {
        loop {
            if let Some(data) = self.decoder.next_data()? {
                let envelope: XrayEnvelope =
                    serde_json::from_str(&data).map_err(|_| XrayClientError::Protocol)?;
                self.accept(&envelope)?;
                return Ok(envelope);
            }
            let chunk = self
                .response
                .chunk()
                .await
                .map_err(|_| XrayClientError::Transport)?
                .ok_or(XrayClientError::Incomplete)?;
            self.decoder.push(&chunk)?;
        }
    }

    fn accept(&mut self, envelope: &XrayEnvelope) -> Result<(), XrayClientError> {
        if envelope.schema_version != XRAY_SCHEMA_VERSION
            || envelope.observation_id.is_empty()
            || envelope.event.correlation.session_id != self.session_id
            || self
                .observation_id
                .as_ref()
                .is_some_and(|id| id != &envelope.observation_id)
        {
            return Err(XrayClientError::Protocol);
        }
        let delivered_through = validate_sequence(envelope, self.last_sequence)?;
        if envelope.event.domain == "xray" && envelope.event.name == "finalized" {
            let finalization: XrayFinalization =
                serde_json::from_value(envelope.event.data.clone())
                    .map_err(|_| XrayClientError::Protocol)?;
            if finalization.observation_id != envelope.observation_id {
                return Err(XrayClientError::Protocol);
            }
            if finalization.final_sequence != envelope.sequence {
                return Err(XrayClientError::Protocol);
            }
            self.finalization = Some(finalization);
            self.finalized = true;
        }
        self.observation_id = Some(envelope.observation_id.clone());
        self.last_sequence = delivered_through;
        Ok(())
    }
}

fn validate_sequence(envelope: &XrayEnvelope, previous: u64) -> Result<u64, XrayClientError> {
    let expected = previous.checked_add(1).ok_or(XrayClientError::Protocol)?;
    if envelope.event.domain == "xray" && envelope.event.name == "gap" {
        let gap: XrayGap = serde_json::from_value(envelope.event.data.clone())
            .map_err(|_| XrayClientError::Protocol)?;
        if gap.first_sequence != expected
            || gap.last_sequence < gap.first_sequence
            || envelope.sequence != gap.first_sequence
        {
            return Err(XrayClientError::Protocol);
        }
        Ok(gap.last_sequence)
    } else if envelope.sequence == expected {
        Ok(envelope.sequence)
    } else {
        Err(XrayClientError::Protocol)
    }
}

/// Incremental bounded SSE decoding. Heartbeats and unknown SSE fields carry no events.
#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<(), XrayClientError> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_SSE_FRAME_BYTES {
            return Err(XrayClientError::Protocol);
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn next_data(&mut self) -> Result<Option<String>, XrayClientError> {
        loop {
            let lf = self
                .buffer
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|end| (end, 2));
            let crlf = self
                .buffer
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|end| (end, 4));
            let Some((end, delimiter)) = (match (lf, crlf) {
                (Some(a), Some(b)) => Some(if a.0 < b.0 { a } else { b }),
                (a, b) => a.or(b),
            }) else {
                return Ok(None);
            };
            let frame =
                std::str::from_utf8(&self.buffer[..end]).map_err(|_| XrayClientError::Protocol)?;
            let mut data = Vec::new();
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.strip_prefix(' ').unwrap_or(value));
                }
            }
            let data = data.join("\n");
            self.buffer.drain(..end + delimiter);
            if !data.is_empty() {
                return Ok(Some(data));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_http_operations_have_request_and_body_deadlines() {
        let client = XrayClient::new("http://127.0.0.1:1234", "test-token").unwrap();
        for path in ["v1/xray/finalize", "v1/xray/payload", "v1/xray/release"] {
            let request = client
                .bounded_post(path)
                .unwrap()
                .json(&serde_json::json!({"schema_version": 1}))
                .build()
                .unwrap();
            assert_eq!(request.timeout(), Some(&Duration::from_secs(15)));
            assert_eq!(request.method(), reqwest::Method::POST);
            assert!(request.headers().get(AUTHORIZATION).unwrap().is_sensitive());
            assert!(request.url().query().is_none());
        }
        let live = client
            .http
            .post(client.url("v1/xray/observe").unwrap())
            .build()
            .unwrap();
        assert_eq!(live.timeout(), None);
    }

    #[test]
    fn payload_references_are_typed_bounded_and_conflict_checked() {
        let reference = bcode_xray_models::XrayPayloadRef {
            schema_version: XRAY_SCHEMA_VERSION,
            payload_id: "payload-1".into(),
            session_id: "session".into(),
            content_type: "text/plain".into(),
            byte_len: 3,
            sha256: hex::encode(Sha256::digest(b"abc")),
        };
        let data = serde_json::json!({"nested": [{"payload": reference}, reference]});
        assert_eq!(payload_references(&data).unwrap(), vec![reference.clone()]);
        assert!(validate_payload_bytes(&reference, b"abc").is_ok());
        assert!(validate_payload_bytes(&reference, b"abd").is_err());
        assert!(validate_payload_bytes(&reference, b"ab").is_err());
        let mut future = reference.clone();
        future.schema_version += 1;
        assert!(payload_references(&serde_json::json!({"payload": future})).is_err());
        let mut unknown = serde_json::to_value(&reference).unwrap();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("future".into(), serde_json::json!(true));
        assert!(payload_references(&unknown).is_err());
        assert!(
            payload_references(
                &serde_json::json!({"payload": {"future_encoding": "missing reference"}})
            )
            .is_err()
        );
        assert!(payload_references(&serde_json::json!({"payload_id": "incomplete"})).is_err());
        let mut conflicting = reference.clone();
        conflicting.byte_len += 1;
        assert!(payload_references(&serde_json::json!([reference, conflicting])).is_err());
        let mut deep = serde_json::json!(null);
        for _ in 0..34 {
            deep = serde_json::json!([deep]);
        }
        assert!(payload_references(&deep).is_err());
    }

    #[test]
    fn origins_reject_remote_credential_and_ambient_discovery() {
        for origin in [
            "https://127.0.0.1:1234",
            "http://example.com:80",
            "http://localhost:80",
            "http://127.0.0.1:80/path",
            "http://secret@127.0.0.1:80",
            "http://127.0.0.1:80/?token=x",
        ] {
            assert!(validate_origin(origin).is_err());
        }
        assert!(validate_origin("http://127.0.0.1:1234").is_ok());
        assert!(validate_origin("http://[::1]:1234").is_ok());
    }

    #[test]
    fn fragmented_sse_preserves_utf8_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        let bytes = "data: 雪\r\ndata: two\r\n\r\n".as_bytes();
        for byte in &bytes[..bytes.len() - 1] {
            decoder.push(&[*byte]).unwrap();
            assert!(decoder.next_data().unwrap().is_none());
        }
        decoder.push(&bytes[bytes.len() - 1..]).unwrap();
        assert_eq!(decoder.next_data().unwrap().as_deref(), Some("雪\ntwo"));
    }

    #[test]
    fn sequence_loss_requires_an_exact_gap_and_duplicates_fail() {
        let mut envelope = XrayEnvelope {
            schema_version: XRAY_SCHEMA_VERSION,
            observation_id: "observer".into(),
            sequence: 2,
            timestamp_unix_ms: 0,
            event: bcode_xray_models::XrayEvent {
                domain: "xray".into(),
                name: "gap".into(),
                correlation: bcode_xray_models::XrayCorrelation::default(),
                sensitive: false,
                data: serde_json::json!({"first_sequence": 2, "last_sequence": 5, "reason": "queue_full"}),
            },
        };
        assert_eq!(validate_sequence(&envelope, 1).unwrap(), 5);
        envelope.sequence = 6;
        envelope.event.name = "future-event".into();
        assert_eq!(validate_sequence(&envelope, 5).unwrap(), 6);
        assert!(validate_sequence(&envelope, 1).is_err());
        assert!(validate_sequence(&envelope, 6).is_err());
        assert!(validate_sequence(&envelope, u64::MAX).is_err());
    }

    #[test]
    fn mixed_line_endings_deliver_in_wire_order() {
        let mut decoder = SseDecoder::default();
        decoder
            .push(b"data: first\r\n\r\ndata: second\n\n")
            .unwrap();
        assert_eq!(decoder.next_data().unwrap().as_deref(), Some("first"));
        assert_eq!(decoder.next_data().unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn credentials_and_transport_diagnostics_do_not_reveal_secrets() {
        let client = XrayClient::new("http://127.0.0.1:1234", "secret-test-token").unwrap();
        assert!(!format!("{client:?}").contains("secret-test-token"));
        assert!(client.authorization.is_sensitive());
        assert!(!XrayClientError::Transport.to_string().contains("127.0.0.1"));
    }

    #[test]
    fn heartbeats_are_not_events_and_oversized_frames_fail() {
        let mut decoder = SseDecoder::default();
        decoder.push(b": keepalive\n\ndata: {}\n\n").unwrap();
        assert_eq!(decoder.next_data().unwrap().as_deref(), Some("{}"));
        assert!(decoder.push(&vec![b'x'; MAX_SSE_FRAME_BYTES + 1]).is_err());
    }
}
