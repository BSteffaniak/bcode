//! Correlate workflow attempts only through exact persisted submission identities.
use bcode_session_models::{SessionEvent, SessionEventKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn project(attempts: &Value, events: &[SessionEvent]) -> Value {
    let mut accepted = BTreeMap::new();
    for event in events {
        if let SessionEventKind::SubmissionAccepted { submission } = &event.kind
            && let Some(identity) = submission
                .admission
                .origin
                .as_ref()
                .and_then(|origin| origin.correlation_id.as_ref())
        {
            accepted
                .entry(identity.clone())
                .or_insert_with(Vec::new)
                .push(json!({
                    "session_id":event.session_id,"accepted_sequence":event.sequence,
                    "submission_id":submission.id,"turn_id":submission.target.turn_id(),
                }));
        }
    }
    let observations: Vec<_> = attempts
        .as_array()
        .into_iter()
        .flatten()
        .map(|attempt| {
            let identity = attempt["dispatch_identity"].as_str().unwrap_or_default();
            let mut matches = accepted.get(identity).cloned().unwrap_or_default();
            matches.sort_by_key(Value::to_string);
            matches.dedup();
            json!({"run_id":attempt["run_id"],"node_id":attempt["node_id"],
            "activation_id":attempt["activation_id"],"attempt":attempt["attempt"],
            "dispatch_identity":identity,"status":attempt["status"],
            "session_turns":matches,"correlation_basis":"persisted_submission_origin",
            "availability":if matches.is_empty() {"not_observed"} else {"observed"}})
        })
        .collect();
    json!({"schema":"bcode.eval.workflow_session_correlations.v1","scope":"retained events only; not workflow history or plugin execution evidence","attempts":observations})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_events_do_not_invent_session_turns() {
        let value = project(
            &json!([{"dispatch_identity":"dispatch","node_id":"plugin"}]),
            &[],
        );
        assert_eq!(value["attempts"][0]["availability"], "not_observed");
        assert_eq!(value["attempts"][0]["session_turns"], json!([]));
    }
}
