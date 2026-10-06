//! Bounded per-session causal index over retained canonical events.
//! This index does not reconstruct model requests from later history.
use bcode_session_models::{SessionEvent, SessionEventKind, SessionTracePayload};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use std::io::Write;

pub fn write(path: &std::path::Path, events: &[SessionEvent]) -> Result<(), super::EvalError> {
    let mut file = std::fs::File::create(path)?;
    for event in events {
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
            // Provider-owned metadata and arbitrary error details are not a portable report.
            if let SessionTracePayload::ModelRequestBuilt { request, .. } = &trace.payload {
                if let Some(value) = payload["model_request_built"].as_object_mut() {
                    value.remove("metadata");
                }
                payload["request_capture"] = json!({"available":request.is_some(),"source":"request-time trace blob; never reconstructed","exported":false});
            }
            json!({"kind":"trace","turn_id":trace.turn_id,"timestamp_ms":trace.timestamp_ms,"phase":trace.phase,"payload":payload})
        }
        SessionEventKind::ModelUsage { turn_id, usage } => {
            json!({"kind":"usage","turn_id":turn_id,"request_id":usage.request_id,"observation_ordinal":usage.observation_ordinal,"terminal":usage.terminal,"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"cached_input_tokens":usage.cached_input_tokens})
        }
        SessionEventKind::RequestContextObserved { observation } => {
            json!({"kind":"context","observation":observation})
        }
        SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        } => json!({"kind":"compaction","compacted_through_sequence":compacted_through_sequence}),
        _ => return Ok(None),
    };
    Ok(Some(
        json!({"schema_version":1,"session_id":event.session_id,"sequence":event.sequence,"timestamp_ms":event.timestamp_ms,"observation":payload}),
    ))
}

pub fn summary(events: &[SessionEvent]) -> Value {
    let mut requests = 0;
    let mut captured = 0;
    let mut tools = BTreeMap::<String, usize>::new();
    for event in events {
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
    json!({"schema_version":1,"session_id":events.first().map(|event|event.session_id),"retained_events":events.len(),"model_requests_observed":requests,"request_blob_references":captured,"missing_request_capture":requests-captured,"tool_invocations":tools,"complete_model_inputs_exported":false,"limitations":["Index covers retained events only; request trace blobs are referenced, not exported.","Usage observations can be cumulative; do not sum rows. Canonical eval telemetry owns accounting.","Context observations describe occupancy, not the complete model-visible input.","Trace presence does not prove complete provider/tool timing or workflow coverage."]})
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
