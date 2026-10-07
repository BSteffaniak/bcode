use super::*;
use bcode_session_models::{
    SessionHistoryCursor, ToolInvocationResult, ToolInvocationResultRecord,
};

fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id,
        sequence,
        timestamp_ms: 1,
        provenance: None,
        kind,
    }
}

fn page(session_id: SessionId, events: Vec<SessionEvent>) -> SessionHistoryPage {
    SessionHistoryPage {
        session_id,
        events,
        compatibility_issues: Vec::new(),
        next_cursor: None,
        has_more: false,
    }
}

fn pair(
    session_id: SessionId,
    name: &str,
    arguments: &str,
    output: &str,
) -> (SessionHistoryPage, SessionHistoryPage) {
    let request = event(
        session_id,
        4,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "invocation".into(),
            producer_plugin_id: Some("fixture.plugin".into()),
            tool_name: name.into(),
            arguments_json: arguments.into(),
            working_directory: Some("/private/working-directory".into()),
        },
    );
    let result = event(
        session_id,
        90,
        SessionEventKind::ToolInvocationResultRecorded {
            record: ToolInvocationResultRecord {
                invocation_id: "invocation".into(),
                model_output: output.into(),
                is_error: false,
                content: Vec::new(),
                presentation: None,
                result: Some(ToolInvocationResult::Text {
                    text: "private semantic payload omitted".into(),
                }),
            },
        },
    );
    (
        page(session_id, vec![request]),
        page(session_id, vec![result]),
    )
}

fn query(session_id: SessionId, byte_limit: usize) -> PairQuery<'static> {
    PairQuery {
        session_id,
        invocation_id: "invocation",
        request_sequence: 4,
        result_sequence: 90,
        byte_limit,
    }
}

#[test]
fn two_tool_shapes_preserve_model_evidence_without_private_fields() {
    let id = SessionId::new();
    for (name, arguments, output) in [
        (
            "fixture.command",
            r#"{"command":"validate"}"#,
            r#"{"exit_code":0,"output":"PASS"}"#,
        ),
        ("fixture.lookup", r#"{"key":"record"}"#, "found record"),
    ] {
        let (request, result) = pair(id, name, arguments, output);
        let evidence = read_pair(&query(id, MAX_BYTES), &request, &result).expect("valid pair");
        assert_eq!(evidence.tool_name, name);
        assert_eq!(evidence.arguments_json.text, arguments);
        assert_eq!(evidence.model_output.text, output);
        assert!(!evidence.model_output.truncated);
        assert_eq!(evidence.producer_capture_complete, None);
        let encoded = serde_json::to_value(evidence).expect("projection serializes");
        assert!(encoded.get("presentation").is_none());
        assert!(encoded.get("content").is_none());
        assert!(encoded.get("result").is_none());
        assert!(encoded.get("working_directory").is_none());
    }
}

#[test]
fn positioned_requests_share_the_same_generic_receipt_contract() {
    let id = SessionId::new();
    let (mut request, result) = pair(id, "fixture", "{}", "ok");
    request.events[0].kind = SessionEventKind::PositionedToolCallRequested {
        turn_id: "private turn".into(),
        output_position: bcode_session_models::TurnOutputPosition::new(0),
        tool_call_id: "invocation".into(),
        producer_plugin_id: None,
        tool_name: "fixture".into(),
        arguments_json: "{}".into(),
        working_directory: None,
    };
    let evidence = read_pair(&query(id, MAX_BYTES), &request, &result).expect("positioned request");
    assert_eq!(evidence.model_output.text, "ok");
    assert_eq!(evidence.producer_plugin_id, None);
}

#[test]
fn discovery_counts_all_events_and_retains_forward_continuation() {
    let id = SessionId::new();
    let (mut requested, _) = pair(id, "fixture", "{}", "ok");
    requested.has_more = true;
    requested.next_cursor = Some(SessionHistoryCursor { sequence: 5 });
    let found = discover(id, &requested, 4, 1).expect("bounded page");
    assert_eq!(found.scanned_events, 1);
    assert_eq!(found.next_sequence, Some(5));
    assert_eq!(found.events[0].role, EventRole::Request);
    assert_eq!(found.events[0].invocation_id, "invocation");
    requested.next_cursor = Some(SessionHistoryCursor { sequence: 6 });
    assert_eq!(discover(id, &requested, 4, 1), Err(EvidenceError::Conflict));
}

#[test]
fn missing_foreign_wrong_sequence_and_invocation_fail_closed() {
    let id = SessionId::new();
    let (request, mut result) = pair(id, "fixture", "{}", "ok");
    assert_eq!(
        read_pair(&query(id, MAX_BYTES), &request, &page(id, vec![])),
        Err(EvidenceError::Missing)
    );
    result.session_id = SessionId::new();
    assert_eq!(
        read_pair(&query(id, MAX_BYTES), &request, &result),
        Err(EvidenceError::Conflict)
    );
    result.session_id = id;
    result.events[0].session_id = SessionId::new();
    assert_eq!(
        read_pair(&query(id, MAX_BYTES), &request, &result),
        Err(EvidenceError::Conflict)
    );
    result.events[0].session_id = id;
    result.events[0].sequence = 91;
    assert_eq!(
        read_pair(&query(id, MAX_BYTES), &request, &result),
        Err(EvidenceError::Conflict)
    );
    result.events[0].sequence = 90;
    let SessionEventKind::ToolInvocationResultRecorded { record } = &mut result.events[0].kind
    else {
        panic!("fixture result")
    };
    record.invocation_id = "different".into();
    assert_eq!(
        read_pair(&query(id, MAX_BYTES), &request, &result),
        Err(EvidenceError::Conflict)
    );
}

#[test]
fn future_versions_and_oversized_pages_are_rejected() {
    let id = SessionId::new();
    let (mut request, result) = pair(id, "fixture", "{}", "ok");
    request.events[0].schema_version += 1;
    assert_eq!(
        read_pair(&query(id, MAX_BYTES), &request, &result),
        Err(EvidenceError::Incompatible)
    );
    assert_eq!(
        discover(id, &request, 4, 1),
        Err(EvidenceError::Incompatible)
    );
    request.events[0].schema_version = CURRENT_SESSION_EVENT_SCHEMA_VERSION;
    request.events.push(request.events[0].clone());
    assert_eq!(
        read_pair(&query(id, MAX_BYTES), &request, &result),
        Err(EvidenceError::LimitExceeded)
    );
}

#[test]
fn discovery_ignores_non_tool_events_but_advances_over_them() {
    let id = SessionId::new();
    let created = event(
        id,
        0,
        SessionEventKind::SessionCreated {
            name: Some("private session title".into()),
            working_directory: "/private".into(),
        },
    );
    let mut history = page(id, vec![created]);
    history.has_more = true;
    history.next_cursor = Some(SessionHistoryCursor { sequence: 1 });
    let discovery = discover(id, &history, 0, 1).expect("non-tool page");
    assert!(discovery.events.is_empty());
    assert_eq!(discovery.scanned_events, 1);
    assert_eq!(discovery.next_sequence, Some(1));
}

#[test]
fn wrong_kind_and_reversed_pair_are_not_receipts() {
    let id = SessionId::new();
    let (request, result) = pair(id, "fixture", "{}", "ok");
    let mut wrong_kind = result.clone();
    wrong_kind.events[0].kind = SessionEventKind::ModelTurnStarted {
        turn_id: "private turn".into(),
    };
    assert_eq!(
        read_pair(&query(id, MAX_BYTES), &request, &wrong_kind),
        Err(EvidenceError::Conflict)
    );
    let mut reversed = query(id, MAX_BYTES);
    reversed.result_sequence = reversed.request_sequence;
    assert_eq!(
        read_pair(&reversed, &request, &result),
        Err(EvidenceError::Conflict)
    );
    assert_eq!(
        read_pair(&query(id, 0), &request, &result),
        Err(EvidenceError::LimitExceeded)
    );
}

#[test]
fn truncation_is_utf8_safe_and_explicit_not_full_process_capture() {
    let id = SessionId::new();
    let (request, result) = pair(id, "fixture", "{}", "évidence");
    let evidence = read_pair(&query(id, 3), &request, &result).expect("bounded text");
    assert_eq!(evidence.arguments_json.retained_bytes, 2);
    assert!(!evidence.arguments_json.truncated);
    assert_eq!(evidence.model_output.retained_bytes, 0);
    assert!(evidence.model_output.truncated);
    assert_eq!(evidence.model_output.original_bytes, "évidence".len());
    assert_eq!(evidence.producer_capture_complete, None);
}
