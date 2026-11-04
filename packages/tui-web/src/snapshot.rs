//! Bounded full-observation acceptance and exact submission retry state.
use std::collections::BTreeSet;

use bcode_session_view_models::{SessionViewActionOutcome, SessionViewSnapshot};
use serde::{Deserialize, Serialize};

/// Maximum encoded submission body (also enforced by the HTTP bridge).
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Maximum full snapshot or action response, before parsing.
pub const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    schema_version: u16,
    epoch: String,
    revision: u64,
    snapshot: SessionViewSnapshot,
    #[serde(default, rename = "policy")]
    policy: Option<serde_json::Value>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    #[serde(rename = "submission_id")]
    identity: String,
    #[serde(default)]
    attempt_id: Option<String>,
    /// Unmodified editor text, retained when native command syntax is normalized.
    #[serde(default)]
    draft_text: Option<String>,
    #[serde(rename = "session_id")]
    session: String,
    text: String,
    placement: String,
    #[serde(default)]
    native_only: bool,
    #[serde(default = "legacy_prepared")]
    prepared: bool,
    #[serde(default)]
    managed: Option<bcode_session_view_models::ManagedComposerRequest>,
    #[serde(default)]
    expected_native_turn_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum BrowserControl {
    CancelTurn {
        turn_id: String,
    },
    Permission {
        permission_id: String,
        approved: bool,
        remember: bool,
    },
    PermissionBatch {
        batch_id: String,
        approved: bool,
    },
    Interaction {
        interaction_id: String,
        input: BrowserInteractionInput,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum BrowserInteractionInput {
    Activate {
        control_id: String,
    },
    Change {
        control_id: String,
        value: serde_json::Value,
    },
    Focus {
        control_id: String,
    },
    Blur {
        control_id: String,
    },
    Navigate {
        direction: BrowserNavigation,
    },
    Submit {
        value: Option<serde_json::Value>,
    },
    Cancel,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BrowserNavigation {
    Next,
    Previous,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BrowserHistoryDirection {
    Older,
    Newer,
    Tail,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    schema_version: u16,
    session: String,
    pending: Option<Submission>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionResponse {
    schema_version: u16,
    outcome: SessionViewActionOutcome,
}

/// Accepted semantic snapshot and one unresolved exact submission.
/// Epoch changes require a new adapter, not silent reuse of old interaction state.
pub struct SnapshotState {
    session: String,
    accepted: Option<Observation>,
    pending: Option<Submission>,
}

impl SnapshotState {
    /// Bind this adapter to one explicitly selected session UUID.
    ///
    /// # Errors
    /// Returns a fixed diagnostic for an invalid session identity.
    pub fn new(session: String) -> Result<Self, &'static str> {
        if !uuid(&session) {
            return Err("invalid session identity");
        }
        Ok(Self {
            session,
            accepted: None,
            pending: None,
        })
    }

    /// Accept an atomic full observation; failure leaves the previous one intact.
    ///
    /// # Errors
    /// Rejects oversized, malformed, stale, wrong-session, changed-epoch,
    /// duplicate-item, conflicting-revision and unsupported-schema observations.
    pub fn accept(&mut self, json: &str) -> Result<bool, &'static str> {
        if json.len() > MAX_RESPONSE_BYTES {
            return Err("response too large");
        }
        let next: Observation = serde_json::from_str(json).map_err(|_| "invalid snapshot")?;
        if next.schema_version != 1
            || next.snapshot.schema_version != SessionViewSnapshot::SCHEMA_VERSION
            || !uuid(&next.epoch)
            || next
                .snapshot
                .session_id
                .as_ref()
                .map(ToString::to_string)
                .as_deref()
                != Some(self.session.as_str())
        {
            return Err("unsupported snapshot identity or schema");
        }
        let mut ids = BTreeSet::new();
        if next
            .snapshot
            .transcript
            .items
            .iter()
            .any(|item| !ids.insert(&item.id))
        {
            return Err("duplicate transcript identity");
        }
        if let Some(previous) = &self.accepted {
            if next.epoch != previous.epoch {
                return Err("server epoch changed; remount required");
            }
            if next.revision < previous.revision {
                return Err("stale snapshot");
            }
            if next.revision == previous.revision {
                return if next.snapshot == previous.snapshot && next.policy == previous.policy {
                    Ok(false)
                } else {
                    Err("conflicting snapshot revision")
                };
            }
        }
        self.accepted = Some(next);
        Ok(true)
    }

    /// Prepare an observation-scoped canonical artifact range; callers name a
    /// semantic item and reference key, never a filesystem path or storage URI.
    ///
    /// # Errors
    /// Rejects foreign references, missing observations and oversized ranges.
    pub fn prepare_artifact(
        &self,
        item_id: &str,
        reference_key: &str,
        offset: u64,
        length: u32,
    ) -> Result<String, &'static str> {
        let accepted = self.accepted.as_ref().ok_or("missing observation")?;
        if length == 0 || length > 128 * 1024 {
            return Err("invalid artifact range");
        }
        let item = accepted
            .snapshot
            .transcript
            .items
            .iter()
            .find(|item| item.id.get() == item_id)
            .ok_or("stale artifact")?;
        let bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool } = &item.kind
        else {
            return Err("unsupported artifact");
        };
        let Some(bcode_session_view_models::ToolResultView::Artifact { artifact }) = &tool.result
        else {
            return Err("unsupported artifact");
        };
        let reference = artifact
            .artifact
            .refs
            .iter()
            .find(|reference| reference.key == reference_key)
            .ok_or("stale artifact")?;
        if reference.byte_len.is_some_and(|length| offset > length) {
            return Err("invalid artifact range");
        }
        let json = serde_json::to_string(&serde_json::json!({
            "session_id": self.session, "epoch": accepted.epoch, "revision": accepted.revision,
            "item_id": item_id, "reference_key": reference_key, "offset": offset, "length": length,
        }))
        .map_err(|_| "invalid artifact request")?;
        if json.len() > MAX_REQUEST_BYTES {
            return Err("request too large");
        }
        Ok(json)
    }

    /// Validate directional convergence before replacing a resident window.
    ///
    /// # Errors
    /// Rejects oversized, malformed, non-progressing or boundary-less windows.
    pub fn validate_history(&self, json: &str, direction: &str) -> Result<(), &'static str> {
        self.prepare_history(direction)?;
        if json.len() > MAX_RESPONSE_BYTES {
            return Err("response too large");
        }
        let next: Observation = serde_json::from_str(json).map_err(|_| "invalid snapshot")?;
        let previous = &self
            .accepted
            .as_ref()
            .ok_or("missing observation")?
            .snapshot
            .transcript;
        let document = &next.snapshot.transcript;
        let progressing = match direction {
            "older" => {
                previous
                    .source_start_sequence
                    .zip(document.source_start_sequence)
                    .is_some_and(|(old, new)| {
                        new < old || (new == old && !document.has_older_history)
                    })
                    || (document.items.is_empty()
                        && !document.has_older_history
                        && document.source_start_sequence.is_none())
            }
            "newer" => {
                previous
                    .source_end_sequence
                    .zip(document.source_end_sequence)
                    .is_some_and(|(old, new)| {
                        new > old || (new == old && !document.has_newer_history)
                    })
                    || (document.items.is_empty()
                        && !document.has_newer_history
                        && document.source_end_sequence.is_none())
            }
            "tail" => !document.has_newer_history,
            _ => false,
        };
        if !progressing {
            return Err("history window did not progress");
        }
        Ok(())
    }

    /// Prepare a bounded semantic history replacement from accepted source metadata.
    /// The host must pass response bytes to `accept`, not merge raw events in JS.
    ///
    /// # Errors
    /// Rejects absent observations, unsupported direction or unavailable boundaries.
    pub fn prepare_history(&self, direction: &str) -> Result<String, &'static str> {
        let accepted = self.accepted.as_ref().ok_or("missing observation")?;
        let direction: BrowserHistoryDirection =
            serde_json::from_value(serde_json::Value::String(direction.to_owned()))
                .map_err(|_| "invalid history direction")?;
        match direction {
            BrowserHistoryDirection::Older
                if !accepted.snapshot.transcript.has_older_history
                    || accepted.snapshot.transcript.source_start_sequence.is_none() =>
            {
                return Err("history boundary unavailable");
            }
            BrowserHistoryDirection::Newer
                if !accepted.snapshot.transcript.has_newer_history
                    || accepted.snapshot.transcript.source_end_sequence.is_none() =>
            {
                return Err("history boundary unavailable");
            }
            _ => {}
        }
        self.browser_request("direction", &direction)
    }

    /// Prepare only a narrow browser permission, turn cancellation, or producer-owned interaction input.
    /// This does not retain a retry receipt or authorize arbitrary session actions.
    ///
    /// # Errors
    /// Rejects malformed/oversized input, unsupported operations and nonpending IDs.
    pub fn prepare_control(&self, json: &str) -> Result<String, &'static str> {
        if json.len() > MAX_REQUEST_BYTES {
            return Err("request too large");
        }
        let action: BrowserControl =
            serde_json::from_str(json).map_err(|_| "invalid browser control")?;
        let snapshot = self.snapshot().ok_or("missing observation")?;
        match &action {
            BrowserControl::CancelTurn { turn_id } => {
                if turn_id.is_empty() || snapshot.runtime.active_turn_id.as_ref() != Some(turn_id) {
                    return Err("stale browser control");
                }
            }
            BrowserControl::Permission {
                permission_id,
                remember,
                ..
            } => {
                if !snapshot.permissions.iter().any(|value| {
                    value.permission_id == *permission_id
                        && !value.resolved
                        && (!*remember || value.can_remember)
                }) {
                    return Err("stale browser control");
                }
            }
            BrowserControl::PermissionBatch { batch_id, .. } => {
                if !snapshot.permissions.iter().any(|value| {
                    !value.resolved
                        && value
                            .batch
                            .as_ref()
                            .is_some_and(|batch| batch.batch_id == *batch_id)
                }) {
                    return Err("stale browser control");
                }
            }
            BrowserControl::Interaction { interaction_id, .. } => {
                if !snapshot
                    .interactions
                    .iter()
                    .any(|value| value.interaction_id == *interaction_id && !value.resolved)
                {
                    return Err("stale browser control");
                }
            }
        }
        self.browser_request("action", &action)
    }

    fn browser_request(
        &self,
        field: &str,
        payload: &impl Serialize,
    ) -> Result<String, &'static str> {
        let accepted = self.accepted.as_ref().ok_or("missing observation")?;
        let mut value = serde_json::json!({ "session_id": self.session, "epoch": accepted.epoch, "revision": accepted.revision });
        value[field] = serde_json::to_value(payload).map_err(|_| "invalid browser request")?;
        let json = serde_json::to_string(&value).map_err(|_| "invalid browser request")?;
        if json.len() > MAX_REQUEST_BYTES {
            return Err("request too large");
        }
        Ok(json)
    }

    /// Validate a narrow control response; it is not an idempotency receipt.
    /// The host must refresh after every attempt, including transport ambiguity.
    ///
    /// # Errors
    /// Rejects oversized, malformed and unrelated application outcomes.
    pub fn accept_control(&self, action_json: &str, json: &str) -> Result<(), &'static str> {
        if action_json.len() > MAX_REQUEST_BYTES || json.len() > MAX_RESPONSE_BYTES {
            return Err("response too large");
        }
        let action: BrowserControl =
            serde_json::from_str(action_json).map_err(|_| "invalid browser control")?;
        let response: ActionResponse =
            serde_json::from_str(json).map_err(|_| "invalid control response")?;
        if response.schema_version != 1 {
            return Err("unsupported control schema");
        }
        match (action, response.outcome) {
            (BrowserControl::CancelTurn { .. }, SessionViewActionOutcome::Cancelled { .. }) => {
                Ok(())
            }
            (
                BrowserControl::Permission { .. },
                SessionViewActionOutcome::PermissionResolved { .. },
            )
            | (
                BrowserControl::PermissionBatch { .. },
                SessionViewActionOutcome::PermissionBatchResolved { .. },
            )
            | (
                BrowserControl::Interaction { .. },
                SessionViewActionOutcome::None
                | SessionViewActionOutcome::InteractionResolved { .. },
            ) => Ok(()),
            _ => Err("unmatched control response"),
        }
    }

    /// Export only the unresolved operation for caller-owned in-memory recovery.
    /// Observations, revisions and presentation geometry never cross a remount.
    ///
    /// # Errors
    /// Returns an error if serialization fails.
    pub fn export_recovery(&self) -> Result<String, &'static str> {
        serde_json::to_string(&Recovery {
            schema_version: 1,
            session: self.session.clone(),
            pending: self.pending.clone(),
        })
        .map_err(|_| "invalid recovery state")
    }

    /// Restore an exact unresolved submission into a fresh adapter only.
    /// This is not a durable resume token and conveys no backend authority.
    ///
    /// # Errors
    /// Rejects oversized, malformed, foreign or unsupported recovery, invalid
    /// submissions, and attempts to overwrite already initialized state.
    pub fn restore_recovery(&mut self, json: &str) -> Result<(), &'static str> {
        if self.accepted.is_some() || self.pending.is_some() || json.len() > MAX_REQUEST_BYTES * 2 {
            return Err("invalid recovery state");
        }
        let recovery: Recovery =
            serde_json::from_str(json).map_err(|_| "invalid recovery state")?;
        if recovery.schema_version != 1 || recovery.session != self.session {
            return Err("invalid recovery identity or schema");
        }
        if let Some(pending) = &recovery.pending
            && (pending.session != self.session
                || !uuid(&pending.identity)
                || pending.text.trim().is_empty()
                || !matches!(pending.placement.as_str(), "steering" | "follow_up")
                || serde_json::to_vec(pending)
                    .map_err(|_| "invalid recovery submission")?
                    .len()
                    > MAX_REQUEST_BYTES)
        {
            return Err("invalid recovery submission");
        }
        if let Some(pending) = &recovery.pending
            && let Some(request) = &pending.managed
        {
            request
                .request()
                .validate()
                .map_err(|_| "invalid recovered native request")?;
            let bcode_session_view_models::ManagedComposerRequestKind::Submit {
                submission, ..
            } = &request.kind
            else {
                return Err("invalid recovered native operation");
            };
            if !pending.prepared
                || submission.submission_id != pending.identity
                || submission.attempt_id
                    != pending.attempt_id.as_deref().unwrap_or(&pending.identity)
                || submission.session_id != pending.session
                || submission.text != pending.text
                || !composer_placement_matches(
                    request,
                    &pending.placement,
                    pending.expected_native_turn_id.as_deref(),
                )
            {
                return Err("recovered native input mismatch");
            }
        }
        self.pending = recovery.pending;
        Ok(())
    }

    /// Revision of the accepted canonical observation, for committed input fencing.
    #[must_use]
    pub fn revision(&self) -> Option<u64> {
        self.accepted.as_ref().map(|value| value.revision)
    }

    /// The unmodified accepted semantic snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Option<&SessionViewSnapshot> {
        self.accepted.as_ref().map(|value| &value.snapshot)
    }

    /// Retain an exact submit payload. Never replace an unresolved operation.
    ///
    /// # Errors
    /// Rejects invalid identity, empty text, absent snapshot and oversized body.
    pub fn prepare_submission(
        &mut self,
        identity: String,
        text: String,
    ) -> Result<String, &'static str> {
        if self.pending.is_some() {
            return self.retry_submission().ok_or("missing submission");
        }
        if !uuid(&identity) || text.trim().is_empty() || self.accepted.is_none() {
            return Err("invalid submission");
        }
        let native_only = text.starts_with("/native steer ") || text.starts_with("/native next ");
        let draft_text = text.clone();
        let (placement, expected_native_turn_id, text) =
            if let Some(rest) = text.strip_prefix("/native steer ") {
                let (expected, text) = rest
                    .split_once(char::is_whitespace)
                    .ok_or("usage: /native steer <expected-native-turn> <text>")?;
                if expected.is_empty() || text.trim().is_empty() {
                    return Err("empty native steering input");
                }
                (
                    "steering".to_owned(),
                    Some(expected.to_owned()),
                    text.trim_start().to_owned(),
                )
            } else if let Some(text) = text.strip_prefix("/native next ") {
                if text.trim().is_empty() {
                    return Err("empty native input");
                }
                ("follow_up".to_owned(), None, text.trim_start().to_owned())
            } else {
                ("follow_up".to_owned(), None, text)
            };
        let next = Submission {
            attempt_id: Some(format!("attempt-{identity}")),
            identity,
            draft_text: Some(draft_text),
            session: self.session.clone(),
            text,
            placement,
            prepared: false,
            managed: None,
            expected_native_turn_id,
            native_only,
        };
        let body = serde_json::to_string(&next).map_err(|_| "invalid submission")?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err("request too large");
        }
        self.pending = Some(next);
        Ok(body)
    }

    /// Exact retained payload, including its original identity, for ambiguous retries.
    #[must_use]
    pub fn retry_submission(&self) -> Option<String> {
        self.pending
            .as_ref()
            .and_then(|pending| serde_json::to_string(pending).ok())
    }

    /// Whether the exact pending operation still needs non-mutating backend preparation.
    #[must_use]
    pub fn needs_preparation(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| !pending.prepared)
    }

    /// Retain preparation before native dispatch; an existing captured envelope is immutable.
    /// # Errors
    /// Rejects malformed, mismatched or non-preparation responses.
    pub fn accept_preparation(&mut self, json: &str) -> Result<(), &'static str> {
        if json.len() > MAX_RESPONSE_BYTES {
            return Err("preparation too large");
        }
        let response: ActionResponse =
            serde_json::from_str(json).map_err(|_| "invalid preparation")?;
        let pending = self.pending.as_mut().ok_or("no pending submission")?;
        if pending.prepared || response.schema_version != 1 {
            return Err("unexpected preparation");
        }
        let SessionViewActionOutcome::SubmissionPrepared { managed } = response.outcome else {
            return Err("invalid preparation outcome");
        };
        if pending.native_only && managed.is_none() {
            return Err("native backend unavailable; no model fallback");
        }
        if let Some(request) = &managed {
            if !pending.prepared && request.request_id != pending.identity {
                return Err("preparation request mismatch");
            }
            let bcode_session_view_models::ManagedComposerRequestKind::Submit {
                submission, ..
            } = &request.kind
            else {
                return Err("invalid preparation operation");
            };
            request
                .request()
                .validate()
                .map_err(|_| "invalid preparation")?;
            if submission.submission_id != pending.identity
                || submission.attempt_id
                    != pending.attempt_id.as_deref().unwrap_or(&pending.identity)
                || submission.session_id != pending.session
                || submission.text != pending.text
                || !composer_placement_matches(
                    request,
                    &pending.placement,
                    pending.expected_native_turn_id.as_deref(),
                )
            {
                return Err("preparation owner mismatch");
            }
        }
        let Some(pending) = self.pending.as_ref() else {
            return Ok(());
        };
        let mut candidate = pending.clone();
        if managed.is_none() && !pending.native_only {
            // Preserve the pre-existing browser model-composer steering policy. The initial
            // next-turn placement is the explicit managed default, not a model policy change.
            "steering".clone_into(&mut candidate.placement);
        }
        candidate.managed = managed.map(|request| *request);
        candidate.prepared = true;
        if serde_json::to_vec(&candidate)
            .map_err(|_| "invalid prepared input")?
            .len()
            > MAX_REQUEST_BYTES
        {
            return Err("prepared input exceeds request budget");
        }
        self.pending = Some(candidate);
        Ok(())
    }

    /// Validate a durable receipt before releasing the retained retry identity.
    /// Returns the accepted draft so a host clears only that exact draft.
    ///
    /// # Errors
    /// Rejects malformed/oversized responses, unsupported schemas, non-durable
    /// outcomes and receipts for a different session or submission.
    pub fn settle_submission(&mut self, json: &str) -> Result<String, &'static str> {
        if json.len() > MAX_RESPONSE_BYTES {
            return Err("response too large");
        }
        let response: ActionResponse =
            serde_json::from_str(json).map_err(|_| "invalid action response")?;
        let pending = self.pending.as_ref().ok_or("no pending submission")?;
        if response.schema_version != 1 {
            return Err("unsupported action schema");
        }
        match response.outcome {
            SessionViewActionOutcome::ManagedSubmissionRecorded { response } => {
                let retained = pending
                    .managed
                    .as_ref()
                    .ok_or("unexpected native receipt")?;
                response
                    .response()
                    .validate_for(&retained.request())
                    .map_err(|_| "native receipt mismatch")?;
                let bcode_session_view_models::ManagedComposerResponseKind::Receipt(receipt) =
                    response.kind
                else {
                    return Err("native delivery unresolved; exact input retained");
                };
                let state =
                    serde_json::to_value(receipt.state).map_err(|_| "invalid delivery state")?;
                if state == "uncertain" || state == "delivery_attempted" {
                    return Err("native delivery unresolved; exact input retained");
                }
            }
            SessionViewActionOutcome::SubmissionAccepted {
                session_id,
                receipt,
            } if pending.managed.is_none()
                && session_id.to_string() == pending.session
                && receipt.submission_id.0 == pending.identity => {}
            _ => return Err("unmatched submission receipt"),
        }
        let text = pending
            .draft_text
            .clone()
            .unwrap_or_else(|| pending.text.clone());
        self.pending = None;
        Ok(text)
    }
}

const fn legacy_prepared() -> bool {
    // Legacy recovery may already have sent a model submission. Never reprepare it as native.
    true
}

fn composer_placement_matches(
    request: &bcode_session_view_models::ManagedComposerRequest,
    placement: &str,
    expected: Option<&str>,
) -> bool {
    let bcode_session_view_models::ManagedComposerRequestKind::Submit { submission, .. } =
        &request.kind
    else {
        return false;
    };
    if submission.submission_id != request.request_id {
        return false;
    }
    let Ok(value) = serde_json::to_value(&submission.placement) else {
        return false;
    };
    match placement {
        "follow_up" => value["kind"] == "next_turn" && expected.is_none(),
        "steering" => {
            value["kind"] == "steer"
                && value["expected_native_turn_id"].as_str() == expected
                && expected.is_some()
        }
        _ => false,
    }
}

fn uuid(value: &str) -> bool {
    value.len() == 36 && uuid::Uuid::parse_str(value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    const SESSION: &str = "00000000-0000-4000-8000-000000000001";
    const EPOCH: &str = "00000000-0000-4000-8000-000000000002";

    fn observation(revision: u64) -> String {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.session_id = Some(SESSION.parse().unwrap());
        serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":revision,"snapshot":snapshot}).to_string()
    }

    #[test]
    fn cancel_control_is_fenced_to_exact_active_turn_and_receipt_kind() {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.session_id = Some(SESSION.parse().unwrap());
        snapshot.runtime.active_turn_id = Some("active".into());
        let mut state = SnapshotState::new(SESSION.into()).unwrap();
        state.accept(&serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":u64::MAX,"snapshot":snapshot}).to_string()).unwrap();
        let action = r#"{"type":"cancel_turn","turn_id":"active"}"#;
        let request = state.prepare_control(action).unwrap();
        assert!(request.contains("18446744073709551615"));
        assert!(
            state
                .prepare_control(r#"{"type":"cancel_turn","turn_id":"stale"}"#)
                .is_err()
        );
        assert!(
            state
                .accept_control(
                    action,
                    r#"{"schema_version":1,"outcome":{"type":"cancelled","cancelled":true}}"#
                )
                .is_ok()
        );
        assert!(
            state
                .accept_control(action, r#"{"schema_version":1,"outcome":{"type":"none"}}"#)
                .is_err()
        );
    }

    #[test]
    fn observation_ordering_is_exact_u64_and_atomic() {
        let mut state = SnapshotState::new(SESSION.into()).unwrap();
        let first = observation(u64::MAX - 1);
        assert_eq!(state.accept(&first), Ok(true));
        assert_eq!(state.accept(&first), Ok(false));
        assert!(state.accept(&observation(1)).is_err());
        let mut conflicting: serde_json::Value = serde_json::from_str(&first).unwrap();
        conflicting["snapshot"]["title"] = "changed".into();
        assert!(state.accept(&conflicting.to_string()).is_err());
        conflicting["epoch"] = SESSION.into();
        assert!(state.accept(&conflicting.to_string()).is_err());
        assert_eq!(state.accept(&observation(u64::MAX)), Ok(true));
        assert!(state.accept(&" ".repeat(MAX_RESPONSE_BYTES + 1)).is_err());
    }

    #[test]
    fn browser_requests_use_exact_u64_observation_and_narrow_authority() {
        let mut state = SnapshotState::new(SESSION.into()).unwrap();
        state.accept(&observation(u64::MAX)).unwrap();
        let request = state.prepare_history("tail").unwrap();
        assert!(request.contains("18446744073709551615"));
        assert!(state.prepare_history("older").is_err());
        assert!(
            state
                .prepare_control(r#"{"type":"invoke_skill","skill_id":"shell"}"#)
                .is_err()
        );
        assert!(state.prepare_control(r#"{"type":"permission","permission_id":"missing","approved":true,"remember":false}"#).is_err());
        let action =
            r#"{"type":"permission","permission_id":"p","approved":false,"remember":false}"#;
        assert!(state.accept_control(action, r#"{"schema_version":1,"outcome":{"type":"permission_resolved","resolved":false}}"#).is_ok());
        assert!(
            state
                .accept_control(
                    action,
                    r#"{"schema_version":1,"outcome":{"type":"cancelled","cancelled":true}}"#
                )
                .is_err()
        );
    }

    #[test]
    fn history_windows_require_directional_progress_without_mutating_on_rejection() {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.session_id = Some(SESSION.parse().unwrap());
        snapshot.transcript.has_older_history = true;
        snapshot.transcript.has_newer_history = true;
        snapshot.transcript.source_start_sequence = Some(100);
        snapshot.transcript.source_end_sequence = Some(200);
        let encode = |snapshot: &SessionViewSnapshot, revision| {
            serde_json::json!({
                "schema_version":1,"epoch":EPOCH,"revision":revision,"snapshot":snapshot
            })
            .to_string()
        };
        let mut state = SnapshotState::new(SESSION.into()).unwrap();
        state.accept(&encode(&snapshot, 1)).unwrap();
        assert!(
            state
                .validate_history(&encode(&snapshot, 2), "older")
                .is_err()
        );
        assert!(
            state
                .validate_history(&encode(&snapshot, 2), "newer")
                .is_err()
        );
        snapshot.transcript.source_start_sequence = Some(50);
        assert!(
            state
                .validate_history(&encode(&snapshot, 2), "older")
                .is_ok()
        );
        snapshot.transcript.source_end_sequence = Some(250);
        assert!(
            state
                .validate_history(&encode(&snapshot, 2), "newer")
                .is_ok()
        );
        assert!(
            state
                .validate_history(&encode(&snapshot, 2), "tail")
                .is_err()
        );
        snapshot.transcript.has_newer_history = false;
        assert!(
            state
                .validate_history(&encode(&snapshot, 2), "tail")
                .is_ok()
        );
        assert_eq!(state.accepted.as_ref().unwrap().revision, 1);
    }

    #[test]
    fn managed_preparation_recovery_keeps_exact_attempt_and_rejects_foreign_receipts() {
        let mut state = SnapshotState::new(SESSION.into()).unwrap();
        state.accept(&observation(1)).unwrap();
        state
            .prepare_submission(EPOCH.into(), "native input".into())
            .unwrap();
        let retained = serde_json::json!({
            "schema_version":1,"request_id":EPOCH,
            "instance":{"instance_id":"native","plugin_id":"native.plugin","executable":"/native","config_namespace":"isolated",
                "auth":{"provider_id":"native","profile":"work","credential_namespace":"isolated","custody":"native_managed"}},
            "kind":{"operation":"submit","request":{
                "target":{"state":"prepared","session":{"open_id":"open","session_id":SESSION,"instance_id":"native","generation":3,"native_version":"1","requested_native_session_id":null}},
                "submission":{"submission_id":EPOCH,"attempt_id":format!("attempt-{EPOCH}"),"session_id":SESSION,"generation":3,"text":"native input","placement":{"kind":"next_turn"}}
            }}
        });
        let typed: Result<bcode_session_view_models::ManagedComposerRequest, _> =
            serde_json::from_value(retained);
        let retained = typed.expect("fixture must match the real portable contract");
        let preparation = serde_json::json!({"schema_version":1,"outcome":{"type":"submission_prepared","managed":retained}});
        let original = state.retry_submission().unwrap();
        let mut foreign = preparation.clone();
        foreign["outcome"]["managed"]["kind"]["request"]["submission"]["attempt_id"] =
            "foreign-attempt".into();
        assert!(state.accept_preparation(&foreign.to_string()).is_err());
        assert_eq!(state.retry_submission().as_deref(), Some(original.as_str()));
        assert!(state.needs_preparation());
        state.accept_preparation(&preparation.to_string()).unwrap();
        assert!(
            state.accept_preparation(&preparation.to_string()).is_err(),
            "captured target is immutable"
        );
        let exact = state.retry_submission().unwrap();
        let mut recovered = SnapshotState::new(SESSION.into()).unwrap();
        recovered
            .restore_recovery(&state.export_recovery().unwrap())
            .unwrap();
        assert_eq!(
            recovered.retry_submission().as_deref(),
            Some(exact.as_str())
        );
        assert!(!recovered.needs_preparation());
        let receipt = serde_json::json!({"schema_version":1,"outcome":{"type":"managed_submission_recorded","response":{
            "schema_version":1,"request_id":EPOCH,"operation":"submit","kind":{"kind":"receipt","response":{
                "submission_id":EPOCH,"attempt_id":format!("attempt-{EPOCH}"),"session_id":SESSION,"generation":3,"state":"uncertain","native_request_id":null
            }}
        }}});
        assert!(recovered.settle_submission(&receipt.to_string()).is_err());
        assert_eq!(
            recovered.retry_submission().as_deref(),
            Some(exact.as_str())
        );
        let mut attempted = receipt.clone();
        attempted["outcome"]["response"]["kind"]["response"]["state"] = "delivery_attempted".into();
        assert!(recovered.settle_submission(&attempted.to_string()).is_err());
        assert_eq!(
            recovered.retry_submission().as_deref(),
            Some(exact.as_str())
        );
        let mut wrong = receipt.clone();
        wrong["outcome"]["response"]["kind"]["response"]["generation"] = 4.into();
        wrong["outcome"]["response"]["kind"]["response"]["state"] = "accepted".into();
        assert!(recovered.settle_submission(&wrong.to_string()).is_err());
        let mut accepted = receipt;
        accepted["outcome"]["response"]["kind"]["response"]["state"] = "accepted".into();
        assert_eq!(
            recovered.settle_submission(&accepted.to_string()).unwrap(),
            "native input"
        );
        assert!(recovered.retry_submission().is_none());
    }

    #[test]
    fn pending_capture_budget_rejects_oversize_without_losing_existing_retry() {
        let mut state = SnapshotState::new(SESSION.into()).unwrap();
        state.accept(&observation(1)).unwrap();
        assert!(
            state
                .prepare_submission(EPOCH.into(), "x".repeat(MAX_REQUEST_BYTES))
                .is_err()
        );
        assert!(state.retry_submission().is_none());
        let exact = state
            .prepare_submission(EPOCH.into(), "bounded input".into())
            .unwrap();
        assert_eq!(
            state
                .prepare_submission(SESSION.into(), "x".repeat(MAX_REQUEST_BYTES))
                .unwrap(),
            exact
        );
        assert_eq!(state.retry_submission().as_deref(), Some(exact.as_str()));
        let mut oversized =
            serde_json::from_str::<serde_json::Value>(&state.export_recovery().unwrap()).unwrap();
        oversized["pending"]["text"] = "x".repeat(MAX_REQUEST_BYTES).into();
        let mut fresh = SnapshotState::new(SESSION.into()).unwrap();
        assert!(fresh.restore_recovery(&oversized.to_string()).is_err());
        assert!(fresh.retry_submission().is_none());
    }

    #[test]
    fn explicit_native_steering_is_retained_and_rejects_missing_turn() {
        let mut state = SnapshotState::new(SESSION.into()).unwrap();
        state.accept(&observation(1)).unwrap();
        let body = state
            .prepare_submission(EPOCH.into(), "/native steer turn-7 hello".into())
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["placement"], "steering");
        assert_eq!(value["expected_native_turn_id"], "turn-7");
        assert_eq!(value["text"], "hello");
        assert_eq!(value["draft_text"], "/native steer turn-7 hello");
        assert_eq!(state.retry_submission().as_deref(), Some(body.as_str()));
    }

    #[test]
    fn recovery_preserves_exact_retry_without_old_observation_authority() {
        let mut old = SnapshotState::new(SESSION.into()).unwrap();
        old.accept(&observation(5)).unwrap();
        let exact = old
            .prepare_submission(EPOCH.into(), "draft 🦀".into())
            .unwrap();
        let recovery = old.export_recovery().unwrap();
        let mut fresh = SnapshotState::new(SESSION.into()).unwrap();
        fresh.restore_recovery(&recovery).unwrap();
        assert_eq!(fresh.retry_submission(), Some(exact));
        assert!(fresh.snapshot().is_none());
        let mut restarted: serde_json::Value = serde_json::from_str(&observation(1)).unwrap();
        restarted["epoch"] = SESSION.into();
        assert_eq!(fresh.accept(&restarted.to_string()), Ok(true));
        assert!(fresh.restore_recovery(&recovery).is_err());
        let mut foreign = SnapshotState::new(EPOCH.into()).unwrap();
        assert!(foreign.restore_recovery(&recovery).is_err());
        assert!(foreign.retry_submission().is_none());
    }

    #[test]
    fn ambiguous_retry_never_changes_payload_and_wrong_receipt_keeps_it() {
        let mut state = SnapshotState::new(SESSION.into()).unwrap();
        state.accept(&observation(1)).unwrap();
        let body = state
            .prepare_submission(EPOCH.into(), "draft 🦀".into())
            .unwrap();
        assert_eq!(
            state
                .prepare_submission(SESSION.into(), "different".into())
                .unwrap(),
            body
        );
        assert!(
            state
                .settle_submission(r#"{"schema_version":1,"outcome":{"type":"none"}}"#)
                .is_err()
        );
        assert_eq!(state.retry_submission().as_deref(), Some(body.as_str()));
        let receipt = serde_json::json!({"schema_version":1,"outcome":{"type":"submission_accepted","session_id":SESSION,"receipt":{"submission_id":EPOCH,"accepted_event_sequence":1}}});
        assert_eq!(
            state.settle_submission(&receipt.to_string()),
            Ok("draft 🦀".into())
        );
        assert!(state.retry_submission().is_none());
    }
}
