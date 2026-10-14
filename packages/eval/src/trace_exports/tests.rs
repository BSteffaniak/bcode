use super::*;
use bcode_session_models::{
    SessionId, SessionTraceBlobRange, TraceBlobCompleteness, TraceBlobRef, TraceRedaction,
};
use sha2::{Digest as _, Sha256};

#[test]
fn all_session_model_inputs_precede_tool_io_under_shared_allowance() {
    use bcode_session_models::{
        SessionEvent, SessionEventKind, SessionTraceEvent, SessionTracePayload, SessionTracePhase,
    };
    use std::cell::RefCell;
    let directory =
        std::env::temp_dir().join(format!("bcode-export-priority-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let blob = TraceBlobRef {
        sha256: hex::encode(Sha256::digest(b"x")),
        path: "opaque".into(),
        content_type: "application/json".into(),
        byte_len: 1,
        redaction: TraceRedaction::Automatic,
        completeness: TraceBlobCompleteness::Complete,
    };
    let event = |session, sequence, model| SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        timestamp_ms: 1,
        provenance: None,
        session_id: session,
        sequence,
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 1,
                turn_id: Some("turn".into()),
                phase: if model {
                    SessionTracePhase::ModelRequestBuilt
                } else {
                    SessionTracePhase::ToolInvocationFinished
                },
                payload: if model {
                    SessionTracePayload::ModelRequestBuilt {
                        provider: "provider".into(),
                        model: "model".into(),
                        agent_id: "worker".into(),
                        message_count: 1,
                        tool_count: 0,
                        system_prompt_chars: 0,
                        prompt_cache_mode: "off".into(),
                        conversation_reuse_mode: "off".into(),
                        uses_previous_provider_response: false,
                        metadata: std::collections::BTreeMap::new(),
                        request: Some(blob.clone()),
                    }
                } else {
                    SessionTracePayload::ToolInvocationFinished {
                        tool_call_id: "tool".into(),
                        duration_ms: 1,
                        is_error: false,
                        output_bytes: 1,
                        output: Some(blob.clone()),
                    }
                },
            }),
        },
    };
    let first = SessionId::new();
    let second = SessionId::new();
    let sessions = vec![
        (
            directory.join("first.jsonl"),
            vec![event(first, 1, false), event(first, 2, true)],
        ),
        (directory.join("second.jsonl"), vec![event(second, 3, true)]),
        (
            directory.join("repeat.jsonl"),
            vec![event(first, 1, false), event(first, 2, true)],
        ),
    ];
    let calls = RefCell::new(Vec::new());
    let invoke = |args: &[String], _: &str, _: std::time::Duration| {
        let sequence = args[3].parse::<u64>().unwrap();
        calls.borrow_mut().push(sequence);
        if sequence == 1 {
            return Err(super::super::EvalError::Validation(
                "allowance exhausted by optional I/O".into(),
            ));
        }
        Ok(serde_json::to_value(SessionTraceBlobRange {
            version: SESSION_TRACE_EXPORT_VERSION,
            session_id: args[2].parse().unwrap(),
            sequence,
            sha256: blob.sha256.clone(),
            content_type: blob.content_type.clone(),
            byte_len: 1,
            redaction: TraceRedaction::Automatic,
            completeness: TraceBlobCompleteness::Complete,
            offset: 0,
            bytes: b"x".to_vec(),
            eof: true,
        })
        .unwrap())
    };
    super::export_cli_batch(&sessions, &invoke).unwrap();
    assert_eq!(*calls.borrow(), [2, 3, 1]);
    let receipts: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("first.captures.json")).unwrap())
            .unwrap();
    assert_eq!(receipts["receipts"][0]["status"], "exported");
    assert_eq!(receipts["receipts"][1]["status"], "unavailable");
    assert_eq!(receipts["receipts"][1]["failure"], "invocation");
    let repeated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("repeat.captures.json")).unwrap())
            .unwrap();
    assert_eq!(receipts, repeated);
    let name = receipts["receipts"][0]["file"].as_str().unwrap();
    assert_eq!(
        std::fs::read(directory.join("repeat.captures").join(name)).unwrap(),
        b"x"
    );
    for (path, _) in sessions {
        let root = path.with_extension("captures");
        for file in std::fs::read_dir(&root).unwrap() {
            std::fs::remove_file(file.unwrap().path()).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
        std::fs::remove_file(path.with_extension("captures.json")).unwrap();
    }
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn cli_capture_pages_large_byte_arrays_and_rejects_corruption() {
    let bytes = vec![255_u8; 140_000];
    let capture = super::super::introspection::CaptureReference {
        session_id: SessionId::new(),
        sequence: 7,
        turn_id: None,
        tool_call_id: None,
        kind: "model_request",
        blob: TraceBlobRef {
            sha256: hex::encode(Sha256::digest(&bytes)),
            path: "opaque".into(),
            content_type: "application/json".into(),
            byte_len: bytes.len() as u64,
            redaction: TraceRedaction::Automatic,
            completeness: TraceBlobCompleteness::Complete,
        },
    };
    let read = |args: &[String], _: &str, _: std::time::Duration| {
        let offset: usize = args[5].parse().unwrap();
        let length: usize = args[7].parse().unwrap();
        let end = (offset + length).min(bytes.len());
        let value = serde_json::to_value(SessionTraceBlobRange {
            version: SESSION_TRACE_EXPORT_VERSION,
            session_id: capture.session_id,
            sequence: 7,
            sha256: capture.blob.sha256.clone(),
            content_type: "application/json".into(),
            byte_len: bytes.len() as u64,
            redaction: TraceRedaction::Automatic,
            completeness: TraceBlobCompleteness::Complete,
            offset: offset as u64,
            bytes: bytes[offset..end].to_vec(),
            eof: end == bytes.len(),
        })
        .unwrap();
        assert!(serde_json::to_vec_pretty(&value).unwrap().len() < 1024 * 1024);
        Ok(value)
    };
    assert_eq!(
        read_cli_capture(&capture, "test-observation", &read)
            .unwrap()
            .bytes,
        bytes
    );
    let corrupt = |args: &[String], label: &str, duration| {
        let mut value = read(args, label, duration)?;
        value["bytes"][0] = json!(0);
        Ok(value)
    };
    assert!(read_cli_capture(&capture, "test-observation", &corrupt).is_err());
    let no_progress = |args: &[String], label: &str, duration| {
        let mut value = read(args, label, duration)?;
        value["bytes"] = json!([]);
        value["eof"] = json!(false);
        Ok(value)
    };
    assert!(read_cli_capture(&capture, "test-observation", &no_progress).is_err());
    let calls = std::cell::Cell::new(0);
    let partial_error = |args: &[String], label: &str, duration| {
        calls.set(calls.get() + 1);
        if calls.get() > 1 {
            Err(super::super::EvalError::Client("unavailable".into()))
        } else {
            read(args, label, duration)
        }
    };
    assert_eq!(
        read_cli_capture(&capture, "partial", &partial_error).unwrap_err(),
        CaptureFailure::Invocation
    );
    assert_eq!(calls.get(), 2);
    for (error, expected) in [
        (
            super::super::EvalError::Validation(
                "post-execution capture allowance exhausted".into(),
            ),
            CaptureFailure::Deadline,
        ),
        (
            super::super::EvalError::Client("SECRET /private/storage deadline exhausted".into()),
            CaptureFailure::Invocation,
        ),
        (
            super::super::EvalError::Json(
                serde_json::from_str::<serde_json::Value>("SECRET").unwrap_err(),
            ),
            CaptureFailure::Decode,
        ),
    ] {
        let failure = CaptureFailure::invocation(&error);
        assert_eq!(failure, expected);
        assert!(!serde_json::to_string(&failure).unwrap().contains("SECRET"));
    }
    let decode = |_: &[String], _: &str, _: std::time::Duration| Ok(json!({"secret":"SECRET"}));
    assert_eq!(
        read_cli_capture(&capture, "decode", &decode).unwrap_err(),
        CaptureFailure::Decode
    );
    assert_eq!(
        read_cli_capture(&capture, "corrupt", &corrupt).unwrap_err(),
        CaptureFailure::Integrity
    );
}

#[test]
fn normalized_receipt_validation_is_exact_and_never_delivery() {
    use bcode_model::lineage::{
        ContextRepresentationObservation, RequestContextLineage, RequestContextRepresentation,
    };
    use bcode_session_models::RequestContextObservationReference;
    let mut request: bcode_model::ModelTurnRequest = serde_json::from_value(json!({
        "session_id":"00000000-0000-0000-0000-000000000001","turn_id":"round","model_id":"test","messages":[]
    })).unwrap();
    request.context_lineage = Some(RequestContextLineage {
        version: 1,
        request_ordinal: 7,
        entries: Vec::new(),
        omitted: 2,
    });
    let observation = ContextRepresentationObservation::new(
        &request,
        "turn".into(),
        3,
        RequestContextRepresentation {
            version: 1,
            request_ordinal: 7,
            entries: Vec::new(),
            omitted: 0,
        },
    )
    .unwrap();
    let bytes = observation.encode(&request, "turn", 3).unwrap();
    let reference = RequestContextObservationReference::new(
        "round".into(),
        7,
        3,
        TraceBlobRef {
            sha256: hex::encode(Sha256::digest(&bytes)),
            path: "opaque".into(),
            content_type: "application/json".into(),
            byte_len: bytes.len() as u64,
            redaction: TraceRedaction::None,
            completeness: TraceBlobCompleteness::Complete,
        },
    )
    .unwrap();
    let request_bytes = serde_json::to_vec(&request).unwrap();
    let report = context_report(
        &reference,
        request.session_id,
        "turn",
        &request_bytes,
        &bytes,
    )
    .unwrap();
    assert_eq!(report["status"], "full_receipt_validated");
    assert_eq!(report["lineage_omitted"], 2);
    assert_eq!(report["delivery_claim"], false);
    assert_eq!(report["optional_sources_certified"], false);
    assert!(
        context_report(
            &reference,
            request.session_id,
            "other",
            &request_bytes,
            &bytes
        )
        .is_err()
    );
    assert!(
        context_report(
            &reference,
            request.session_id,
            "turn",
            &request_bytes,
            &bytes[..bytes.len() - 1]
        )
        .is_err()
    );
    let mut foreign = reference;
    foreign.model_request_sequence = 4;
    assert!(context_report(&foreign, request.session_id, "turn", &request_bytes, &bytes).is_err());
}

struct ObservationFixture {
    session: SessionId,
    root: std::path::PathBuf,
    request: bcode_model::ModelTurnRequest,
    reference: bcode_session_models::RequestContextObservationReference,
    observation: bcode_model::lineage::ContextRepresentationObservation,
    request_bytes: Vec<u8>,
    observation_bytes: Vec<u8>,
    receipts: Vec<serde_json::Value>,
    producer: SessionEvent,
    observed: SessionEvent,
}

fn observation_fixture() -> ObservationFixture {
    use bcode_model::lineage::{
        ContextRepresentationObservation, RequestContextLineage, RequestContextRepresentation,
    };
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE,
        RequestContextObservationReference, SessionEventKind, SessionTraceEvent,
        SessionTracePayload, SessionTracePhase,
    };
    let session = SessionId::new();
    let root = std::env::temp_dir().join(format!("bcode-receipt-consumer-{session}"));
    std::fs::create_dir_all(&root).unwrap();
    let mut request: bcode_model::ModelTurnRequest = serde_json::from_value(json!({
        "session_id":session,"turn_id":"round","model_id":"test","messages":[]
    }))
    .unwrap();
    request.context_lineage = Some(RequestContextLineage {
        version: 1,
        request_ordinal: 7,
        entries: Vec::new(),
        omitted: 2,
    });
    let observation = ContextRepresentationObservation::new(
        &request,
        "turn".into(),
        3,
        RequestContextRepresentation {
            version: 1,
            request_ordinal: 7,
            entries: Vec::new(),
            omitted: 0,
        },
    )
    .unwrap();
    let request_bytes = serde_json::to_vec(&request).unwrap();
    let observation_bytes = observation.encode(&request, "turn", 3).unwrap();
    let blob = |bytes: &[u8]| TraceBlobRef {
        sha256: hex::encode(Sha256::digest(bytes)),
        path: "opaque".into(),
        content_type: "application/json".into(),
        byte_len: bytes.len() as u64,
        redaction: TraceRedaction::None,
        completeness: TraceBlobCompleteness::Complete,
    };
    let request_blob = blob(&request_bytes);
    let observation_blob = blob(&observation_bytes);
    let reference =
        RequestContextObservationReference::new("round".into(), 7, 3, observation_blob.clone())
            .unwrap();
    let make_event = |sequence, payload| SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id: session,
        sequence,
        timestamp_ms: 1,
        provenance: None,
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 1,
                turn_id: Some("turn".into()),
                phase: SessionTracePhase::ModelRequestBuilt,
                payload,
            }),
        },
    };
    let producer = make_event(
        3,
        SessionTracePayload::ModelRequestBuilt {
            provider: "test".into(),
            model: "test".into(),
            agent_id: "test".into(),
            message_count: 0,
            tool_count: 0,
            system_prompt_chars: 0,
            prompt_cache_mode: "off".into(),
            conversation_reuse_mode: "off".into(),
            uses_previous_provider_response: false,
            metadata: std::collections::BTreeMap::default(),
            request: Some(request_blob.clone()),
        },
    );
    let observed = make_event(
        4,
        SessionTracePayload::ProviderEvent {
            event_type: REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE.into(),
            detail: Some(reference.encode().unwrap()),
        },
    );
    let exported = |sequence, blob: &TraceBlobRef| {
        json!({"session_id":session,"sequence":sequence,"status":"exported",
        "file":format!("{session}-{sequence}.blob"),"sha256":blob.sha256,"byte_len":blob.byte_len,
        "completeness":blob.completeness,"redaction":blob.redaction})
    };
    std::fs::write(root.join(format!("{session}-3.blob")), &request_bytes).unwrap();
    std::fs::write(root.join(format!("{session}-4.blob")), &observation_bytes).unwrap();
    let receipts = vec![exported(3, &request_blob), exported(4, &observation_blob)];
    ObservationFixture {
        session,
        root,
        request,
        reference,
        observation,
        request_bytes,
        observation_bytes,
        receipts,
        producer,
        observed,
    }
}

#[test]
fn retained_export_files_validate_origin_and_reject_conflicting_observations() {
    use bcode_session_models::SessionEventKind;
    let ObservationFixture {
        session,
        root,
        request,
        reference,
        observation,
        request_bytes,
        observation_bytes,
        receipts,
        producer,
        observed,
    } = observation_fixture();
    let events = vec![producer.clone(), observed.clone()];
    let mut validated = receipts.clone();
    validate_context_exports(&events, &root, &mut validated);
    assert_eq!(
        validated[1]["context_representation_validation"]["status"],
        "full_receipt_validated"
    );
    assert_eq!(
        validated[1]["context_representation_validation"]["optional_sources_certified"],
        false
    );
    let mut conflicting = receipts.clone();
    validate_context_exports(
        &[producer.clone(), observed.clone(), observed.clone()],
        &root,
        &mut conflicting,
    );
    assert_eq!(
        conflicting[1]["context_representation_validation"]["status"],
        "conflicting_observation_events"
    );
    let mut foreign_turn = observed;
    if let SessionEventKind::TraceEvent { trace } = &mut foreign_turn.kind {
        trace.turn_id = Some("foreign".into());
    }
    let mut invalid = receipts.clone();
    validate_context_exports(&[producer, foreign_turn], &root, &mut invalid);
    assert_eq!(
        invalid[1]["context_representation_validation"]["status"],
        "foreign_producer_event"
    );
    let mut future = serde_json::to_value(&observation).unwrap();
    future["version"] = json!(2);
    let future_bytes = serde_json::to_vec(&future).unwrap();
    assert!(context_report(&reference, session, "turn", &request_bytes, &future_bytes).is_err());
    let mut foreign_request = request;
    foreign_request.turn_id = "other-round".into();
    assert!(
        context_report(
            &reference,
            session,
            "turn",
            &serde_json::to_vec(&foreign_request).unwrap(),
            &observation_bytes
        )
        .is_err()
    );
    std::fs::write(root.join(format!("{session}-4.blob")), b"truncated").unwrap();
    let mut invalid = receipts;
    validate_context_exports(&events, &root, &mut invalid);
    assert_eq!(
        invalid[1]["context_representation_validation"]["status"],
        "export_integrity_failed"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn optional_certification_requires_nonempty_complete_explicit_representation() {
    use bcode_model::lineage::{
        ContextLineageEntry, ContextRepresentationDisposition as D, ContextRepresentationEntry,
        ContextRepresentationObservation, RequestContextLineage, RequestContextRepresentation,
    };
    use bcode_session_models::RequestContextObservationReference;
    let mut request: bcode_model::ModelTurnRequest = serde_json::from_value(json!({
        "session_id":"00000000-0000-0000-0000-000000000001","turn_id":"round","model_id":"test",
        "messages":[{"role":"user","content":[{"type":"text","text":"same"},{"type":"text","text":"same"}]}]
    })).unwrap();
    request.context_lineage = Some(RequestContextLineage {
        version: 1,
        request_ordinal: 7,
        omitted: 0,
        entries: vec![
            ContextLineageEntry {
                source_ordinal: 10,
                operation_ordinal: 8,
                mandatory: true,
                message_index: 0,
                block_index: 0,
            },
            ContextLineageEntry {
                source_ordinal: 11,
                operation_ordinal: 8,
                mandatory: false,
                message_index: 0,
                block_index: 1,
            },
        ],
    });
    let report = |request: &bcode_model::ModelTurnRequest, optional: Option<D>| {
        let mut entries = vec![ContextRepresentationEntry {
            source_ordinal: 10,
            disposition: D::Unknown,
        }];
        if let Some(disposition) = optional {
            entries.push(ContextRepresentationEntry {
                source_ordinal: 11,
                disposition,
            });
        }
        let receipt = RequestContextRepresentation {
            version: 1,
            request_ordinal: 7,
            omitted: u64::from(optional.is_none()),
            entries,
        };
        let observation =
            ContextRepresentationObservation::new(request, "turn".into(), 3, receipt).unwrap();
        let bytes = observation.encode(request, "turn", 3).unwrap();
        let reference = RequestContextObservationReference::new(
            "round".into(),
            7,
            3,
            TraceBlobRef {
                sha256: hex::encode(Sha256::digest(&bytes)),
                path: "opaque".into(),
                content_type: "application/json".into(),
                byte_len: bytes.len() as u64,
                redaction: TraceRedaction::None,
                completeness: TraceBlobCompleteness::Complete,
            },
        )
        .unwrap();
        context_report(
            &reference,
            request.session_id,
            "turn",
            &serde_json::to_vec(request).unwrap(),
            &bytes,
        )
        .unwrap()
    };
    let represented = report(&request, Some(D::Represented));
    assert_eq!(represented["optional_sources_certified"], true);
    assert_eq!(represented["optional_representation"], "all_represented");
    assert_eq!(represented["optional"]["represented"], 1);
    assert_eq!(represented["mandatory"]["unknown"], 1);
    assert_eq!(represented["entries"][1]["source_ordinal"], 11);
    assert_eq!(represented["delivery_claim"], false);
    assert_eq!(represented["quality_claim"], false);
    for (disposition, status) in [
        (Some(D::Unknown), "unknown"),
        (Some(D::Dropped), "dropped"),
        (None, "unknown"),
    ] {
        let incomplete = report(&request, disposition);
        assert_eq!(incomplete["optional_sources_certified"], false);
        assert_eq!(incomplete["optional_representation"], status);
    }
    request.context_lineage.as_mut().unwrap().omitted = 1;
    let partial = report(&request, Some(D::Represented));
    assert_eq!(partial["optional_sources_certified"], false);
    assert_eq!(partial["optional_representation"], "incomplete_lineage");
}

#[test]
fn observation_facade_rejects_invalid_windows_before_export() {
    let session = SessionId::new();
    let event = SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id: session,
        sequence: 1,
        timestamp_ms: 1,
        provenance: None,
        kind: bcode_session_models::SessionEventKind::TraceEvent {
            trace: Box::new(bcode_session_models::SessionTraceEvent {
                timestamp_ms: 1,
                turn_id: Some("turn".into()),
                phase: bcode_session_models::SessionTracePhase::ModelProviderEvent,
                payload: bcode_session_models::SessionTracePayload::ProviderEvent {
                    event_type: "diagnostic".into(),
                    detail: None,
                },
            }),
        },
    };
    assert!(validate_context_window(session, std::slice::from_ref(&event)).is_ok());
    assert!(validate_context_window(SessionId::new(), std::slice::from_ref(&event)).is_err());
    assert!(validate_context_window(session, &vec![event.clone(); 3201]).is_err());
    assert!(validate_context_window(session, &[event.clone(), event]).is_err());
    assert!(validate_context_window(session, &[]).is_ok());
}
