use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionId, SessionTokenUsage, SessionTraceEvent,
    SessionTracePayload, SessionTracePhase, TraceBlobCompleteness, TraceBlobRef, TraceRedaction,
};
use serde_json::json;

#[test]
fn export_receipts_fail_closed_and_preserve_incomplete_captures() {
    let session = SessionId::new();
    let blob = TraceBlobRef {
        sha256: "a".repeat(64),
        path: "opaque".into(),
        content_type: "application/json".into(),
        byte_len: 12,
        redaction: TraceRedaction::Automatic,
        completeness: TraceBlobCompleteness::Complete,
    };
    let mut event = event(
        session,
        1,
        SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 1,
                turn_id: Some("turn".into()),
                phase: SessionTracePhase::ModelRequestBuilt,
                payload: SessionTracePayload::ModelRequestBuilt {
                    provider: "provider".into(),
                    model: "model".into(),
                    agent_id: "agent".into(),
                    message_count: 1,
                    tool_count: 0,
                    system_prompt_chars: 0,
                    prompt_cache_mode: "off".into(),
                    conversation_reuse_mode: "off".into(),
                    uses_previous_provider_response: false,
                    metadata: std::collections::BTreeMap::default(),
                    request: Some(blob),
                },
            }),
        },
    );
    let receipt = json!({"session_id":session,"sequence":1,"status":"exported","sha256":"a".repeat(64),
        "byte_len":12,"completeness":"complete","redaction":"automatic","file":"secret-file"});
    let mut document = json!({"schema":"bcode.eval.trace_exports.v1","receipts":[receipt]});
    let value = super::summary_with_exports(std::slice::from_ref(&event), &document);
    assert_eq!(value["complete_model_inputs_exported"], true);
    assert!(!value.to_string().contains("secret-file"));
    document["receipts"] = json!([receipt, receipt]);
    assert_eq!(
        super::summary_with_exports(std::slice::from_ref(&event), &document)["complete_model_inputs_exported"],
        false
    );
    document["schema"] = json!("future");
    assert_eq!(
        super::summary_with_exports(std::slice::from_ref(&event), &document)["export_receipt_schema_supported"],
        false
    );
    if let SessionEventKind::TraceEvent { trace } = &mut event.kind
        && let SessionTracePayload::ModelRequestBuilt {
            request: Some(blob),
            ..
        } = &mut trace.payload
    {
        blob.completeness = TraceBlobCompleteness::Truncated;
    }
    document = json!({"schema":"bcode.eval.trace_exports.v1","receipts":[{
        "session_id":session,"sequence":1,"status":"exported","sha256":"a".repeat(64),
        "byte_len":12,"completeness":"truncated","redaction":"automatic"}]});
    assert_eq!(
        super::summary_with_exports(&[event], &document)["complete_model_inputs_exported"],
        false
    );
}

fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id,
        sequence,
        timestamp_ms: sequence,
        provenance: None,
        kind,
    }
}

fn usage(
    session: SessionId,
    sequence: u64,
    request: Option<&str>,
    ordinal: u32,
    terminal: bool,
) -> SessionEvent {
    event(
        session,
        sequence,
        SessionEventKind::ModelUsage {
            turn_id: "turn".into(),
            usage: SessionTokenUsage {
                request_id: request.map(str::to_owned),
                observation_ordinal: ordinal,
                terminal,
                input_tokens: Some(10),
                ..SessionTokenUsage::default()
            },
        },
    )
}

#[test]
fn canonical_usage_selection_is_terminal_and_session_scoped() {
    let session = SessionId::new();
    let terminal = usage(session, 2, Some("r"), 2, true);
    let events = vec![
        usage(session, 1, Some("r"), 1, false),
        terminal.clone(),
        terminal,
        usage(session, 3, Some("r"), 3, false),
        usage(SessionId::new(), 1, Some("r"), 1, false),
    ];
    let summary = super::summary(&events);
    assert_eq!(summary["duplicate_events"], 1);
    let selected = summary["selected_usage"].as_array().unwrap();
    assert_eq!(selected.len(), 2);
    let terminal = selected
        .iter()
        .find(|row| row["session_id"] == json!(session))
        .unwrap();
    assert_eq!(terminal["observation_ordinal"], 2);
    assert_eq!(terminal["terminal"], true);
}

#[test]
fn absent_request_ids_are_not_coalesced_or_inferred() {
    let session = SessionId::new();
    let events = vec![
        usage(session, 1, None, 0, false),
        usage(session, 2, None, 0, false),
    ];
    assert_eq!(
        super::summary(&events)["selected_usage"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(super::project(&events[0]).unwrap().unwrap()["correlation"]["request_id"].is_null());
}

#[test]
fn capture_manifest_preserves_truncation_and_does_not_open_locators() {
    let blob = TraceBlobRef {
        sha256: "invalid".into(),
        path: "/never/open/secret".into(),
        content_type: "application/json".into(),
        byte_len: 12,
        redaction: TraceRedaction::Automatic,
        completeness: TraceBlobCompleteness::Truncated,
    };
    let event = event(
        SessionId::new(),
        1,
        SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 1,
                turn_id: None,
                phase: SessionTracePhase::ToolInvocationFinished,
                payload: SessionTracePayload::ToolInvocationFinished {
                    tool_call_id: "tool".into(),
                    duration_ms: 7,
                    is_error: false,
                    output_bytes: 42,
                    output: Some(blob),
                },
            }),
        },
    );
    let rows = super::referenced_blobs(&[event.clone(), event.clone()]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "tool_output");
    let summary = super::summary(std::slice::from_ref(&event));
    assert_eq!(
        summary["capture_manifest"][0]["capture"]["status"],
        "malformed_reference"
    );
    assert_eq!(
        summary["capture_manifest"][0]["capture"]["completeness"],
        "truncated"
    );
    assert!(!summary.to_string().contains("/never/open"));
    assert!(
        !super::project(&event)
            .unwrap()
            .unwrap()
            .to_string()
            .contains("/never/open")
    );
}

#[test]
fn provider_errors_and_retry_prose_are_not_portable() {
    let event = event(
        SessionId::new(),
        1,
        SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 1,
                turn_id: Some("turn".into()),
                phase: SessionTracePhase::ModelProviderRoundFinished,
                payload: SessionTracePayload::ProviderRound {
                    provider_turn_id: Some("provider-turn".into()),
                    provider: "provider".into(),
                    round: Some(1),
                    stop_reason: None,
                    duration_ms: Some(8),
                    error: Some("credential=secret".into()),
                },
            }),
        },
    );
    let row = super::project(&event).unwrap().unwrap();
    assert!(!row.to_string().contains("credential"));
    assert_eq!(
        row["observation"]["payload"]["provider_round"]["has_error"],
        true
    );
    assert_eq!(row["correlation"]["provider_turn_id"], "provider-turn");
    assert!(row["correlation"]["request_id"].is_null());
}
