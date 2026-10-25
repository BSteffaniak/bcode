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
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    #[serde(rename = "submission_id")]
    identity: String,
    #[serde(rename = "session_id")]
    session: String,
    text: String,
    placement: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum BrowserControl {
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
                return if next.snapshot == previous.snapshot {
                    Ok(false)
                } else {
                    Err("conflicting snapshot revision")
                };
            }
        }
        self.accepted = Some(next);
        Ok(true)
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

    /// Prepare only a narrow browser permission or producer-owned interaction input.
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
                || pending.placement != "steering"
                || serde_json::to_vec(pending)
                    .map_err(|_| "invalid recovery submission")?
                    .len()
                    > MAX_REQUEST_BYTES)
        {
            return Err("invalid recovery submission");
        }
        self.pending = recovery.pending;
        Ok(())
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
        let next = Submission {
            identity,
            session: self.session.clone(),
            text,
            placement: "steering".to_owned(),
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
            SessionViewActionOutcome::SubmissionAccepted {
                session_id,
                receipt,
            } if session_id.to_string() == pending.session
                && receipt.submission_id.0 == pending.identity => {}
            _ => return Err("unmatched submission receipt"),
        }
        let text = pending.text.clone();
        self.pending = None;
        Ok(text)
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
