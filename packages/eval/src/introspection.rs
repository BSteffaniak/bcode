//! Bounded per-session causal index over retained canonical events.
//! This index does not reconstruct model requests from later history.
use bcode_session_models::{SessionEvent, SessionEventKind, SessionTracePayload};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

mod correlation;
mod exports;
pub use correlation::CaptureReference;
#[cfg(test)]
#[path = "introspection/tests.rs"]
mod correlation_tests;
/// Enumerate captured blobs for a bounded application export; never opens trace locators.
#[must_use]
pub fn referenced_blobs(events: &[SessionEvent]) -> Vec<CaptureReference> {
    correlation::referenced_blobs(events)
}

use std::io::Write;

pub fn write(path: &std::path::Path, events: &[SessionEvent]) -> Result<(), super::EvalError> {
    let mut file = std::fs::File::create(path)?;
    let mut seen = BTreeSet::new();
    for event in events {
        if !seen.insert((event.session_id, event.sequence)) {
            continue;
        }
        if let Some(row) = project(event)? {
            writeln!(file, "{}", serde_json::to_string(&row)?)?;
        }
    }
    Ok(())
}

fn project(event: &SessionEvent) -> Result<Option<Value>, super::EvalError> {
    let payload = match &event.kind {
        SessionEventKind::TraceEvent { trace } => {
            let mut payload = serde_json::to_value(&trace.payload)?;
            sanitize_trace(&mut payload);
            match &trace.payload {
                SessionTracePayload::ModelRequestBuilt { request, .. } => {
                    payload["request_capture"] = correlation::capture(request.as_ref());
                    payload["request_capture"]["available"] = json!(request.is_some());
                }
                SessionTracePayload::ToolInvocationStarted { arguments, .. } => {
                    payload["arguments_capture"] = correlation::capture(arguments.as_ref());
                }
                SessionTracePayload::ToolInvocationFinished { output, .. } => {
                    payload["output_capture"] = correlation::capture(output.as_ref());
                }
                _ => {}
            }
            json!({"kind":"trace","turn_id":trace.turn_id,"timestamp_ms":trace.timestamp_ms,"phase":trace.phase,"payload":payload})
        }
        SessionEventKind::ModelUsage { turn_id, usage } => {
            json!({"kind":"usage","turn_id":turn_id,"request_id":usage.request_id,"observation_ordinal":usage.observation_ordinal,"terminal":usage.terminal,"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"cached_input_tokens":usage.cached_input_tokens})
        }
        SessionEventKind::RequestContextObserved { observation } => {
            json!({"kind":"context","request_id":observation.request.request_id,
                "provider":observation.request.provider_plugin_id,"model":observation.request.effective_model_id,
                "turn_id":observation.request.model_turn_id,"round":observation.request.round,
                "context_epoch":observation.request.context_epoch,
                "context_through_sequence":observation.context_through_sequence,
                "context_tokens":observation.context_tokens,"local_estimate":observation.local_estimate})
        }
        SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        } => json!({"kind":"compaction","compacted_through_sequence":compacted_through_sequence}),
        SessionEventKind::ModelTurnStarted { turn_id } => {
            json!({"kind":"turn_started","turn_id":turn_id})
        }
        SessionEventKind::ModelTurnFinished {
            turn_id, outcome, ..
        } => json!({"kind":"turn_finished","turn_id":turn_id,"outcome":outcome}),
        SessionEventKind::ModelTurnCancelRequested { turn_id, .. } => {
            json!({"kind":"turn_cancel_requested","turn_id":turn_id})
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            ..
        }
        | SessionEventKind::PositionedToolCallRequested {
            tool_call_id,
            tool_name,
            ..
        } => json!({"kind":"tool_requested","tool_call_id":tool_call_id,"tool_name":tool_name}),
        SessionEventKind::ToolInvocationResultRecorded { record } => {
            json!({"kind":"tool_result","tool_call_id":record.invocation_id,"is_error":record.is_error,"model_output_bytes":record.model_output.len(),"structured_content_count":record.content.len()})
        }
        SessionEventKind::PermissionRequested {
            permission_id,
            tool_call_id,
            ..
        } => {
            json!({"kind":"permission_requested","permission_id":permission_id,"tool_call_id":tool_call_id})
        }
        SessionEventKind::PermissionResolved {
            permission_id,
            approved,
        } => {
            json!({"kind":"permission_resolved","permission_id":permission_id,"approved":approved})
        }
        _ => return Ok(None),
    };
    Ok(Some(
        json!({"schema_version":2,"session_id":event.session_id,"sequence":event.sequence,"timestamp_ms":event.timestamp_ms,"correlation":correlation::correlation(event),"observation":payload}),
    ))
}

/// Allow typed timing/identity/count fields, never provider metadata, prose or blob locators.
fn sanitize_trace(payload: &mut Value) {
    match payload {
        Value::Object(fields) => {
            if let Some(error) = fields.get("error") {
                let failed = !error.is_null();
                fields.insert("has_error".into(), json!(failed));
            }
            for key in [
                "metadata",
                "error",
                "detail",
                "reason",
                "message",
                "request",
                "arguments",
                "output",
                "stop_reason",
            ] {
                fields.remove(key);
            }
            for value in fields.values_mut() {
                sanitize_trace(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                sanitize_trace(value);
            }
        }
        _ => {}
    }
}

/// Merge verified application export receipts without reading blobs or reconstructing inputs.
/// Unknown schemas, missing/conflicting receipts and incomplete captures fail closed.
#[must_use]
pub fn summary_with_exports(events: &[SessionEvent], receipts: &Value) -> Value {
    exports::apply(events, receipts)
}

pub fn summary(events: &[SessionEvent]) -> Value {
    let mut requests = 0;
    let mut captured = 0;
    let mut tools = BTreeMap::<String, usize>::new();
    let mut seen = BTreeSet::new();
    for event in events {
        if !seen.insert((event.session_id, event.sequence)) {
            continue;
        }
        if let SessionEventKind::TraceEvent { trace } = &event.kind {
            match &trace.payload {
                SessionTracePayload::ModelRequestBuilt { request, .. } => {
                    requests += 1;
                    captured += usize::from(request.is_some());
                }
                SessionTracePayload::ToolInvocationStarted { tool_name, .. } => {
                    *tools.entry(tool_name.clone()).or_default() += 1;
                }
                _ => {}
            }
        }
    }
    let references = referenced_blobs(events);
    let mut capture_events = BTreeSet::new();
    let capture_manifest: Vec<_> = events
        .iter()
        .filter(|event| capture_events.insert((event.session_id, event.sequence)))
        .filter_map(|event| {
            let SessionEventKind::TraceEvent { trace } = &event.kind else {
                return None;
            };
            let (kind, tool_call_id, blob) = match &trace.payload {
                SessionTracePayload::ModelRequestBuilt { request, .. } => {
                    ("model_request", None, request.as_ref())
                }
                SessionTracePayload::ToolInvocationStarted {
                    tool_call_id,
                    arguments,
                    ..
                } => ("tool_arguments", Some(tool_call_id), arguments.as_ref()),
                SessionTracePayload::ToolInvocationFinished {
                    tool_call_id,
                    output,
                    ..
                } => ("tool_output", Some(tool_call_id), output.as_ref()),
                _ => return None,
            };
            Some(
                json!({"session_id":event.session_id,"sequence":event.sequence,
            "turn_id":trace.turn_id,"tool_call_id":tool_call_id,"kind":kind,
            "capture":correlation::capture(blob)}),
            )
        })
        .collect();
    json!({"schema_version":2,"capture_manifest":capture_manifest,"blob_references":references.len(),
        "selected_usage":correlation::selected_usage(events),
        "capture_gaps":{
            "model_settings":"only fields observed at request time; full settings require verified exact export",
            "snapshots":"not exported; occupancy is not a snapshot",
            "request_build_identity":"request-build traces lack request IDs; no inferred usage/context joins",
            "provider_timing":"round durations only when recorded; queue/retry decomposition unavailable",
            "workflow_handoffs":"requires workflow semantic history and execution-session provenance",
            "retention":"caller-retained events only; no completeness claim for full history"},
        "session_id":events.first().map(|event|event.session_id),"retained_events":events.len(),
        "unique_events":seen.len(),"duplicate_events":events.len()-seen.len(),
        "model_requests_observed":requests,"request_blob_references":captured,"missing_request_capture":requests-captured,
        "tool_invocations":tools,"complete_model_inputs_exported":false,
        "limitations":["Capture references are unverified; export receipts are required.","Usage observations can be cumulative; do not sum rows. Canonical eval telemetry owns accounting.","Context observations describe occupancy, not the complete model-visible input.","Trace presence does not prove complete provider/tool timing or workflow coverage."]})
}

#[cfg(test)]
mod tests {
    #[test]
    fn request_and_tool_observations_keep_sequence_and_capture_gaps() {
        use bcode_session_models::{
            SessionEvent, SessionEventKind, SessionTraceEvent, SessionTracePayload,
            SessionTracePhase,
        };
        let event = SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            timestamp_ms: 42,
            provenance: None,
            session_id: bcode_session_models::SessionId::new(),
            sequence: 7,
            kind: SessionEventKind::TraceEvent {
                trace: Box::new(SessionTraceEvent {
                    timestamp_ms: 42,
                    turn_id: Some("turn".into()),
                    phase: SessionTracePhase::ModelRequestBuilt,
                    payload: SessionTracePayload::ModelRequestBuilt {
                        provider: "provider".into(),
                        model: "model".into(),
                        agent_id: "worker".into(),
                        message_count: 3,
                        tool_count: 2,
                        system_prompt_chars: 100,
                        prompt_cache_mode: "off".into(),
                        conversation_reuse_mode: "off".into(),
                        uses_previous_provider_response: false,
                        metadata: std::collections::BTreeMap::from([(
                            "private".into(),
                            "not exported".into(),
                        )]),
                        request: None,
                    },
                }),
            },
        };
        let projected = super::project(&event).unwrap().unwrap();
        assert_eq!(projected["sequence"], 7);
        assert_eq!(
            projected["observation"]["payload"]["request_capture"]["available"],
            false
        );
        assert!(
            projected["observation"]["payload"]["model_request_built"]
                .get("metadata")
                .is_none()
        );
        assert_eq!(super::summary(&[event])["missing_request_capture"], 1);
    }
    #[test]
    fn missing_capture_is_reported_without_fabricating_requests() {
        let value = super::summary(&[]);
        assert_eq!(value["complete_model_inputs_exported"], false);
        assert_eq!(value["model_requests_observed"], 0);
    }
}
