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

#[derive(Clone, Serialize)]
struct Submission {
    #[serde(rename = "submission_id")]
    identity: String,
    #[serde(rename = "session_id")]
    session: String,
    text: String,
    placement: &'static str,
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
            placement: "steering",
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
