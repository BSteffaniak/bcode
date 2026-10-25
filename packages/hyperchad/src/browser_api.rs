//! Bounded application-semantic JSON routes, independent of the HTML renderer.
//!
//! Every successful read is a full replacement, including after reconnect. The epoch and
//! revision identify observations, not durable stream offsets. Errors are JSON envelopes
//! (the `HyperChad` content boundary does not expose HTTP status/header control).

use std::collections::BTreeMap;

use bcode_session_models::SessionId;
use bcode_session_view_models::{PromptPlacementView, SessionViewAction, SessionViewSnapshot};
use hyperchad::renderer::Content;
use hyperchad::router::RouteRequest;
use serde::{Deserialize, Serialize};

use crate::HyperChadAppState;

pub fn with_routes(
    router: hyperchad::router::Router,
    state: HyperChadAppState,
) -> hyperchad::router::Router {
    let submit_state = state.clone();
    router
        .with_route("/api/session-view/submit-message", move |request| {
            let state = submit_state.clone();
            async move { state.browser_submit(request).await }
        })
        .with_route(
            hyperchad::router::RoutePath::LiteralPrefix("/api/session-view/".to_owned()),
            move |request| {
                let state = state.clone();
                async move { state.browser_snapshot(request).await }
            },
        )
}

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CACHED_SESSIONS: usize = 8;

pub struct BrowserApiState {
    epoch: SessionId,
    revision: u64,
    snapshots: BTreeMap<SessionId, (u64, SessionViewSnapshot)>,
}

impl Default for BrowserApiState {
    fn default() -> Self {
        Self {
            epoch: SessionId::new(),
            revision: 0,
            snapshots: BTreeMap::new(),
        }
    }
}

impl BrowserApiState {
    fn observe(&mut self, session_id: SessionId, snapshot: SessionViewSnapshot) -> Option<u64> {
        if let Some((revision, previous)) = self.snapshots.get(&session_id)
            && previous == &snapshot
        {
            return Some(*revision);
        }
        self.revision = self.revision.checked_add(1)?;
        if self.snapshots.len() == MAX_CACHED_SESSIONS && !self.snapshots.contains_key(&session_id)
        {
            let oldest = self
                .snapshots
                .iter()
                .min_by_key(|(_, (revision, _))| revision)
                .map(|(id, _)| *id);
            if let Some(oldest) = oldest {
                self.snapshots.remove(&oldest);
            }
        }
        self.snapshots.insert(session_id, (self.revision, snapshot));
        Some(self.revision)
    }
}

/// Full application observation for browser-local rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSessionSnapshot {
    /// Browser application wire version (currently 1).
    pub schema_version: u16,
    /// Backend observation lifetime; replace local state when this changes.
    pub epoch: SessionId,
    /// Stable within the epoch for an unchanged complete observation.
    pub revision: u64,
    /// Complete replacement, never a patch or durable resume token.
    pub snapshot: SessionViewSnapshot,
}

/// Existing-session submission with canonical retry/conflict semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserSubmitMessage {
    /// Retain this identity and the exact request on ambiguous failures.
    pub submission_id: String,
    /// Existing target session; session creation is deliberately excluded.
    pub session_id: SessionId,
    /// Nonblank prompt, within the total request byte limit.
    pub text: String,
    /// Shared queue placement policy.
    #[serde(default)]
    pub placement: PromptPlacementView,
}

/// Successful application action response; acceptance is not execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserActionResponse {
    /// Browser application wire version (currently 1).
    pub schema_version: u16,
    /// Canonical semantic result, including a durable acceptance receipt.
    pub outcome: bcode_session_view_models::SessionViewActionOutcome,
}

fn parse_submission(body: &[u8]) -> Result<BrowserSubmitMessage, &'static str> {
    if body.len() > MAX_REQUEST_BYTES {
        return Err("request_too_large");
    }
    let message: BrowserSubmitMessage =
        serde_json::from_slice(body).map_err(|_| "invalid_request")?;
    if message.submission_id.is_empty()
        || message.submission_id.len() > 128
        || !message
            .submission_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        || message.text.trim().is_empty()
    {
        return Err("invalid_request");
    }
    Ok(message)
}

/// Bound serialization itself rather than allocating an unbounded intermediate JSON value.
struct BoundedJson(Vec<u8>);

impl std::io::Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_RESPONSE_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("projection exceeds response limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_content(value: &impl Serialize) -> Content {
    let mut writer = BoundedJson(Vec::new());
    if serde_json::to_writer(&mut writer, value).is_err() {
        return error("response_too_large");
    }
    Content::Raw {
        data: writer.0.into(),
        content_type: "application/json".to_owned(),
    }
}

fn error(code: &str) -> Content {
    Content::Raw {
        data: format!("{{\"schema_version\":1,\"error\":\"{code}\"}}")
            .into_bytes()
            .into(),
        content_type: "application/json".to_owned(),
    }
}

fn submission_error(error: &bcode_client::ClientError) -> &'static str {
    // Only explicitly pre-admission failures establish rejection. Generic server
    // errors can occur after persistence; treat them like an ambiguous transport.
    match error {
        bcode_client::ClientError::Server { code, .. }
            if matches!(
                code.as_str(),
                "invalid_request" | "invalid_turn_admission" | "steering_unavailable"
            ) =>
        {
            "submission_rejected"
        }
        _ => "submission_outcome_unknown",
    }
}

impl HyperChadAppState {
    pub(super) async fn browser_snapshot(&self, request: RouteRequest) -> Content {
        if !self.authorizes(&request) {
            return error("unauthorized");
        }
        if request.method.as_ref() != "GET" {
            return error("method_not_allowed");
        }
        let Some(session_id) = request
            .path
            .strip_prefix("/api/session-view/")
            .and_then(crate::parse_session_id)
        else {
            return error("invalid_session");
        };
        // Serialize observations so concurrent attaches cannot publish older results after newer ones.
        let mut observations = self.browser_api.lock().await;
        let Ok(snapshot) = self.session_snapshot(session_id).await else {
            return error("session_unavailable");
        };
        // Reject oversized observations before retaining them in the bounded cache.
        let mut probe = BoundedJson(Vec::new());
        if serde_json::to_writer(&mut probe, &snapshot).is_err() {
            return error("response_too_large");
        }
        drop(probe);
        let Some(revision) = observations.observe(session_id, snapshot.clone()) else {
            return error("revision_exhausted");
        };
        json_content(&BrowserSessionSnapshot {
            schema_version: 1,
            epoch: observations.epoch,
            revision,
            snapshot,
        })
    }

    pub(super) async fn browser_submit(&self, request: RouteRequest) -> Content {
        if !self.authorizes(&request) {
            return error("unauthorized");
        }
        if request.method.as_ref() != "POST" {
            return error("method_not_allowed");
        }
        if !request.content_type().is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim() == "application/json")
        }) {
            return error("invalid_content_type");
        }
        let message = match parse_submission(request.body.as_deref().map_or(&[], AsRef::as_ref)) {
            Ok(message) => message,
            Err(code) => return error(code),
        };
        // No session creation: ambiguous retry must not create another session. Canonical
        // submission admission owns identity, exact-payload conflict checks and durable receipts.
        let action = SessionViewAction::SubmitMessage {
            submission_id: bcode_session_models::SubmissionId(message.submission_id),
            session_id: Some(message.session_id),
            launch_working_directory: None,
            text: message.text,
            placement: message.placement,
            execution: Box::default(),
        };
        match bcode_session_view::execute_session_view_action(&self.client, action).await {
            Ok(outcome) => json_content(&BrowserActionResponse {
                schema_version: 1,
                outcome,
            }),
            Err(error_value) => error(submission_error(&error_value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn observations_are_stable_changed_and_bounded() {
        let mut state = BrowserApiState::default();
        let session = SessionId::new();
        let snapshot = SessionViewSnapshot::empty();
        let first = state.observe(session, snapshot.clone()).unwrap();
        assert_eq!(state.observe(session, snapshot.clone()), Some(first));
        let mut changed = snapshot.clone();
        changed.title = Some("changed".to_owned());
        assert!(state.observe(session, changed).unwrap() > first);
        for _ in 0..MAX_CACHED_SESSIONS {
            state.observe(SessionId::new(), snapshot.clone()).unwrap();
        }
        assert_eq!(state.snapshots.len(), MAX_CACHED_SESSIONS);
        assert!(!state.snapshots.contains_key(&session));
        assert!(state.observe(session, snapshot).unwrap() > first);
    }

    #[test]
    fn new_backend_has_a_distinct_epoch() {
        assert_ne!(
            BrowserApiState::default().epoch,
            BrowserApiState::default().epoch
        );
    }

    #[test]
    fn submission_rejects_oversize_unknown_fields_and_missing_identity() {
        assert!(parse_submission(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
        let valid = serde_json::json!({
            "session_id": SessionId::new(), "submission_id": "retry-1", "text": "hello"
        });
        assert!(parse_submission(&serde_json::to_vec(&valid).unwrap()).is_ok());
        for invalid in [
            serde_json::json!({"extra": true}),
            serde_json::json!({"submission_id": ""}),
            serde_json::json!({"session_id": null}),
            serde_json::json!({"text": "   "}),
        ] {
            let mut value = valid.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(invalid.as_object().unwrap().clone());
            assert!(parse_submission(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }

    #[tokio::test]
    async fn routes_reject_unauthorized_and_wrong_method_without_backend_access() {
        let state = HyperChadAppState::new(bcode_client::BcodeClient::default_endpoint(), "secret");
        let request = || {
            RouteRequest::from_path(
                "/api/session-view/not-a-session?token=secret",
                hyperchad::router::RequestInfo::default(),
            )
        };
        let decode = |content: Content| {
            let Content::Raw { data, .. } = content else {
                panic!("expected raw JSON")
            };
            serde_json::from_slice::<serde_json::Value>(&data).unwrap()
        };
        let mut unauthorized = request();
        unauthorized.query.clear();
        assert_eq!(
            decode(state.browser_snapshot(unauthorized.clone()).await)["error"],
            "unauthorized"
        );
        assert_eq!(
            decode(state.browser_submit(unauthorized).await)["error"],
            "unauthorized"
        );
        assert_eq!(
            decode(state.browser_snapshot(request()).await)["error"],
            "invalid_session"
        );
        assert_eq!(
            decode(state.browser_submit(request()).await)["error"],
            "method_not_allowed"
        );
        let mut post = request();
        post.method = "POST".parse().unwrap();
        assert_eq!(
            decode(state.browser_snapshot(post.clone()).await)["error"],
            "method_not_allowed"
        );
        assert_eq!(
            decode(state.browser_submit(post.clone()).await)["error"],
            "invalid_content_type"
        );
        post.headers
            .insert("content-type".to_owned(), "application/json".to_owned());
        post.body = Some(std::sync::Arc::new(
            vec![b' '; MAX_REQUEST_BYTES + 1].into(),
        ));
        assert_eq!(
            decode(state.browser_submit(post).await)["error"],
            "request_too_large"
        );
    }

    #[test]
    fn submission_errors_distinguish_rejection_from_ambiguous_receipt_without_details() {
        let rejected = bcode_client::ClientError::Server {
            code: "invalid_turn_admission".to_owned(),
            message: "private token and provider detail".to_owned(),
        };
        assert_eq!(submission_error(&rejected), "submission_rejected");
        assert_eq!(
            submission_error(&bcode_client::ClientError::Server {
                code: "session_operation_failed".to_owned(),
                message: "potential post-commit failure".to_owned(),
            }),
            "submission_outcome_unknown"
        );
        assert_eq!(
            submission_error(&bcode_client::ClientError::UnexpectedResponse),
            "submission_outcome_unknown"
        );
        assert_eq!(
            submission_error(&bcode_client::ClientError::RequestTimeout {
                timeout: std::time::Duration::from_secs(1),
            }),
            "submission_outcome_unknown"
        );
    }

    #[test]
    fn serialization_stops_at_bound_without_partial_response() {
        let mut writer = BoundedJson(Vec::new());
        assert!(writer.write_all(&vec![b'x'; MAX_RESPONSE_BYTES]).is_ok());
        assert!(writer.write_all(b"x").is_err());
        assert_eq!(writer.0.len(), MAX_RESPONSE_BYTES);
        let response = json_content(&"x".repeat(MAX_RESPONSE_BYTES));
        let Content::Raw { data, .. } = response else {
            panic!("expected raw JSON")
        };
        let value: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(value["error"], "response_too_large");
    }
}
