//! Bounded correlation over caller-retained canonical events; never reads session storage.
use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionId, SessionTracePayload, TraceBlobRef,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Sensitive reference passed only to a bounded application export operation.
/// `blob.path` is an opaque domain locator, never a filesystem access grant.
#[derive(Debug, Clone)]
pub struct CaptureReference {
    pub session_id: SessionId,
    pub sequence: u64,
    pub turn_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub kind: &'static str,
    pub blob: TraceBlobRef,
}

/// Enumerate unique request-time captures without opening or validating their bytes.
#[must_use]
pub fn referenced_blobs(events: &[SessionEvent]) -> Vec<CaptureReference> {
    let mut seen = BTreeSet::new();
    events
        .iter()
        .filter(|event| seen.insert((event.session_id, event.sequence)))
        .filter_map(|event| {
            let SessionEventKind::TraceEvent { trace } = &event.kind else {
                return None;
            };
            let (kind, tool_call_id, blob) = match &trace.payload {
                SessionTracePayload::ModelRequestBuilt {
                    request: Some(blob),
                    ..
                } => ("model_request", None, blob),
                SessionTracePayload::ToolInvocationStarted {
                    tool_call_id,
                    arguments: Some(blob),
                    ..
                } => ("tool_arguments", Some(tool_call_id.clone()), blob),
                SessionTracePayload::ToolInvocationFinished {
                    tool_call_id,
                    output: Some(blob),
                    ..
                } => ("tool_output", Some(tool_call_id.clone()), blob),
                _ => return None,
            };
            Some(CaptureReference {
                session_id: event.session_id,
                sequence: event.sequence,
                turn_id: trace.turn_id.clone(),
                tool_call_id,
                kind,
                blob: blob.clone(),
            })
        })
        .collect()
}

pub(super) fn capture(blob: Option<&TraceBlobRef>) -> Value {
    blob.map_or_else(|| json!({"status":"not_captured","exported":false}), |blob| {
        // Do not serialize locators or claim a reference has been verified/exported.
        let valid_digest = blob.sha256.len() == 64 && blob.sha256.bytes().all(|byte| byte.is_ascii_hexdigit());
        json!({"status":if valid_digest { "referenced_unverified" } else { "malformed_reference" },
            "byte_len":blob.byte_len,"completeness":blob.completeness,"redaction":blob.redaction,
            "exported":false})
    })
}

pub(super) fn correlation(event: &SessionEvent) -> Value {
    let mut value = json!({"turn_id":null,"request_id":null,"tool_call_id":null,"provider_turn_id":null,
        "basis":"canonical_event_only"});
    match &event.kind {
        SessionEventKind::ModelTurnStarted { turn_id }
        | SessionEventKind::ModelTurnFinished { turn_id, .. }
        | SessionEventKind::ModelTurnCancelRequested { turn_id, .. } => {
            value["turn_id"] = json!(turn_id);
        }
        SessionEventKind::ModelUsage { turn_id, usage } => {
            value["turn_id"] = json!(turn_id);
            value["request_id"] = json!(usage.request_id);
        }
        SessionEventKind::RequestContextObserved { observation } => {
            value["request_id"] = json!(observation.request.request_id);
            value["turn_id"] = json!(observation.request.model_turn_id);
            value["round"] = json!(observation.request.round);
        }
        SessionEventKind::ToolCallRequested { tool_call_id, .. } => {
            value["tool_call_id"] = json!(tool_call_id);
        }
        SessionEventKind::PositionedToolCallRequested {
            turn_id,
            tool_call_id,
            ..
        } => {
            value["turn_id"] = json!(turn_id);
            value["tool_call_id"] = json!(tool_call_id);
        }
        SessionEventKind::ToolInvocationResultRecorded { record } => {
            value["tool_call_id"] = json!(record.invocation_id);
        }
        SessionEventKind::TraceEvent { trace } => {
            value["turn_id"] = json!(trace.turn_id);
            match &trace.payload {
                SessionTracePayload::ProviderRound {
                    provider_turn_id, ..
                } => value["provider_turn_id"] = json!(provider_turn_id),
                SessionTracePayload::ToolInvocationStarted { tool_call_id, .. }
                | SessionTracePayload::ToolInvocationFinished { tool_call_id, .. }
                | SessionTracePayload::ToolPolicyEvaluated { tool_call_id, .. }
                | SessionTracePayload::ToolPermissionWait { tool_call_id, .. } => {
                    value["tool_call_id"] = json!(tool_call_id);
                }
                _ => {}
            }
        }
        _ => {}
    }
    value
}

/// Select cumulative usage through the existing telemetry rule, scoped by session.
pub(super) fn selected_usage(events: &[SessionEvent]) -> Value {
    let mut seen = BTreeSet::new();
    let mut sessions =
        BTreeMap::<SessionId, Vec<(Option<String>, bcode_session_models::SessionTokenUsage)>>::new(
        );
    for event in events {
        if !seen.insert((event.session_id, event.sequence)) {
            continue;
        }
        if let SessionEventKind::ModelUsage { usage, .. } = &event.kind {
            super::super::record_cache_round(sessions.entry(event.session_id).or_default(), usage);
        }
    }
    json!(sessions.into_iter().flat_map(|(session, usages)| usages.into_iter().map(move |(_, usage)| {
        json!({"session_id":session,"request_id":usage.request_id,"observation_ordinal":usage.observation_ordinal,
            "terminal":usage.terminal,"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,
            "cached_input_tokens":usage.cached_input_tokens})
    })).collect::<Vec<_>>())
}
