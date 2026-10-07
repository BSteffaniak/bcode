use super::*;
use bcode_session_models::SessionId;
use serde_json::json;

fn event(id: SessionId, sequence: u64) -> SessionEvent {
    SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence,
        timestamp_ms: 1,
        session_id: id,
        provenance: None,
        kind: SessionEventKind::SystemMessage {
            text: "fixture".into(),
        },
    }
}

fn page(id: SessionId, sequences: &[u64], next: Option<u64>) -> Value {
    json!({"session_id":id,"events":sequences.iter().map(|s|event(id,*s)).collect::<Vec<_>>(),
        "compatibility_issues":[],"has_more":next.is_some(),"next_cursor":next.map(|sequence|json!({"sequence":sequence}))})
}

#[test]
fn verified_non_model_parent_does_not_hide_missing_execution_boundaries() {
    let complete = json!({"model_requests_observed":2,"complete_model_inputs_exported":true});
    let empty = json!({"model_requests_observed":0,"complete_model_inputs_exported":false});
    let mut parent = event(SessionId::new(), 0);
    parent.kind = SessionEventKind::SessionCreated {
        name: None,
        working_directory: std::path::PathBuf::new(),
    };
    let mut coverage = ModelCoverage::default();
    coverage.observe(&[parent], &empty, false, true);
    coverage.observe(&[], &complete, true, true);
    assert!(coverage.complete());
    assert_eq!(coverage.non_model_sessions, 1);
    assert_eq!(coverage.applicable_sessions, 1);
    coverage.observe(&[], &empty, true, true);
    assert!(!coverage.complete());
    assert_eq!(coverage.unknown_sessions, 1);
    assert_eq!(coverage.missing_boundaries, 0);
    let mut failed_tail = ModelCoverage::default();
    failed_tail.observe(&[], &empty, false, false);
    assert!(!failed_tail.complete());
    assert_eq!(failed_tail.unknown_sessions, 1);
}

#[test]
fn missing_exports_or_incomplete_history_remain_incomplete() {
    for (summary, complete_history) in [
        (
            json!({"model_requests_observed":1,"complete_model_inputs_exported":false}),
            true,
        ),
        (
            json!({"model_requests_observed":1,"complete_model_inputs_exported":true}),
            false,
        ),
        (json!({"model_requests_observed":0}), false),
    ] {
        let mut coverage = ModelCoverage::default();
        coverage.observe(&[], &summary, false, complete_history);
        assert!(!coverage.complete());
        assert_eq!(coverage.non_model_sessions, 0);
    }
    let id = SessionId::new();
    let mut started = event(id, 0);
    started.kind = SessionEventKind::ModelTurnStarted {
        turn_id: "turn".into(),
    };
    let mut coverage = ModelCoverage::default();
    coverage.observe(
        &[started],
        &json!({"model_requests_observed":0}),
        false,
        true,
    );
    assert!(!coverage.complete());
    assert_eq!(coverage.missing_boundaries, 1);
}

#[test]
fn parent_first_page_is_reused_once_and_continuation_preserves_events() {
    let id = SessionId::new();
    let mut cached = Some(page(id, &[0, 1], Some(2)));
    let calls = std::cell::Cell::new(0);
    let first = first_or_fetch(&mut cached, || -> Result<Value, ()> {
        calls.set(calls.get() + 1);
        panic!("cached page must not be fetched again");
    })
    .expect("cached");
    let first = history_page(&first, &id.to_string(), 0).expect("first");
    assert_eq!(calls.get(), 0);
    let after = first.next.expect("continuation");
    let second = first_or_fetch(&mut cached, || -> Result<Value, ()> {
        calls.set(calls.get() + 1);
        Ok(page(id, &[2, 3], None))
    })
    .expect("fetch");
    let second = history_page(&second, &id.to_string(), after).expect("second");
    assert_eq!(calls.get(), 1);
    assert!(second.next.is_none());
    let combined = first
        .events
        .into_iter()
        .chain(second.events)
        .collect::<Vec<_>>();
    assert_eq!(combined, (0..4).map(|s| event(id, s)).collect::<Vec<_>>());
}

#[test]
fn cached_eof_fetches_appended_model_request_and_missing_capture_is_incomplete() {
    use bcode_session_models::{SessionTraceEvent, SessionTracePayload, SessionTracePhase};

    let id = SessionId::new();
    let mut created = event(id, 0);
    created.kind = SessionEventKind::SessionCreated {
        name: None,
        working_directory: std::path::PathBuf::new(),
    };
    let cached = json!({"session_id":id,"events":[created],"compatibility_issues":[],"has_more":false,"next_cursor":null});
    let first = history_page(&cached, &id.to_string(), 0).expect("prefix");
    let after = continuation(&first, true).expect("cached EOF requires fresh tail");
    assert_eq!(after, 1);
    let mut started = event(id, 1);
    started.kind = SessionEventKind::ModelTurnStarted {
        turn_id: "turn".into(),
    };
    let mut request = event(id, 2);
    request.kind = SessionEventKind::TraceEvent {
        trace: Box::new(SessionTraceEvent {
            timestamp_ms: 1,
            turn_id: Some("turn".into()),
            phase: SessionTracePhase::ModelRequestBuilt,
            payload: SessionTracePayload::ModelRequestBuilt {
                provider: "fixture".into(),
                model: "fixture".into(),
                agent_id: "fixture".into(),
                message_count: 1,
                tool_count: 0,
                system_prompt_chars: 0,
                prompt_cache_mode: "off".into(),
                conversation_reuse_mode: "off".into(),
                uses_previous_provider_response: false,
                metadata: std::collections::BTreeMap::default(),
                request: None,
            },
        }),
    };
    let fresh = json!({"session_id":id,"events":[started,request],"compatibility_issues":[],"has_more":false,"next_cursor":null});
    let calls = std::cell::Cell::new(0);
    let value = first_or_fetch(&mut None, || -> Result<Value, ()> {
        calls.set(calls.get() + 1);
        Ok(fresh)
    })
    .expect("fresh fetch");
    let tail = history_page(&value, &id.to_string(), after).expect("fresh tail");
    assert_eq!(calls.get(), 1);
    assert!(continuation(&tail, false).is_none());
    let events = first
        .events
        .into_iter()
        .chain(tail.events)
        .collect::<Vec<_>>();
    let summary = crate::introspection::summary_with_exports(
        &events,
        &json!({"schema":"bcode.eval.trace_exports.v1","receipts":[]}),
    );
    let mut coverage = ModelCoverage::default();
    coverage.observe(&events, &summary, false, true);
    assert!(!coverage.complete());
    assert_eq!(coverage.applicable_sessions, 1);
    assert_eq!(coverage.non_model_sessions, 0);
}

#[test]
fn incompatible_duplicate_gapped_or_foreign_pages_fail_closed() {
    let id = SessionId::new();
    for sequences in [vec![0, 0], vec![0, 2], vec![1]] {
        assert!(history_page(&page(id, &sequences, None), &id.to_string(), 0).is_none());
    }
    let mut incompatible = page(id, &[0], None);
    incompatible["compatibility_issues"] = json!([{"unknown":"future"}]);
    assert!(history_page(&incompatible, &id.to_string(), 0).is_none());
    assert!(history_page(&page(SessionId::new(), &[0], None), &id.to_string(), 0).is_none());
    assert!(history_page(&page(id, &[0], Some(4)), &id.to_string(), 0).is_none());
    assert!(history_page(&page(id, &[], Some(0)), &id.to_string(), 0).is_none());
    let mut future = page(id, &[0], None);
    future["events"][0]["schema_version"] = json!(CURRENT_SESSION_EVENT_SCHEMA_VERSION + 1);
    assert!(history_page(&future, &id.to_string(), 0).is_none());
}
