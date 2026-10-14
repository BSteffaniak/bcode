//! Behavioral coverage for confined retained trace reads.

use super::*;
use bcode_session_models::{
    SessionId, SessionTraceEvent, SessionTracePhase, TraceBlobCompleteness, TraceRedaction,
};

fn request(session_id: SessionId) -> ReadSessionTraceBlobRequest {
    ReadSessionTraceBlobRequest {
        version: SESSION_TRACE_EXPORT_VERSION,
        session_id,
        sequence: 7,
        offset: 0,
        length: 3,
    }
}

fn fixture(bytes: &[u8]) -> (tempfile::TempDir, ReadSessionTraceBlobRequest, TraceBlobRef) {
    let root = tempfile::tempdir().expect("trace root");
    let session_id = SessionId::new();
    let store = super::super::TraceStore::new(root.path().to_path_buf());
    let reference = store
        .write_blob(session_id, "tool-output", "text/plain", bytes, 0)
        .expect("retained blob");
    (root, request(session_id), reference)
}

fn event(request: &ReadSessionTraceBlobRequest, reference: TraceBlobRef) -> SessionEvent {
    SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: request.sequence,
        timestamp_ms: 0,
        session_id: request.session_id,
        provenance: None,
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 0,
                turn_id: None,
                phase: SessionTracePhase::ToolInvocationFinished,
                payload: SessionTracePayload::ToolInvocationFinished {
                    tool_call_id: "call".to_string(),
                    duration_ms: 1,
                    is_error: false,
                    output_bytes: usize::try_from(reference.byte_len).expect("length"),
                    output: Some(reference),
                },
            }),
        },
    }
}

#[tokio::test]
async fn invalid_requests_fail_before_session_lookup() {
    let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    let mut request = request(SessionId::new());
    request.length = 0;
    assert_eq!(
        read_range(&state, request.clone()).await,
        Err(ReadTraceBlobError::InvalidRange),
    );
    request.length = 1;
    request.version += 1;
    assert_eq!(
        read_range(&state, request).await,
        Err(ReadTraceBlobError::Unsupported),
    );
}

#[test]
fn request_wire_contract_rejects_unknown_fields() {
    let request = request(SessionId::new());
    let mut value = serde_json::to_value(&request).expect("request value");
    assert_eq!(
        serde_json::from_value::<ReadSessionTraceBlobRequest>(value.clone())
            .expect("request roundtrip"),
        request,
    );
    value["path"] = serde_json::json!("arbitrary/private/path");
    assert!(serde_json::from_value::<ReadSessionTraceBlobRequest>(value).is_err());
}

#[test]
fn observation_reference_requires_turn_and_earlier_request_event() {
    use bcode_session_models::{
        REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE, RequestContextObservationReference,
    };
    let (_root, request, mut reference) = fixture(b"payload");
    reference.content_type = "application/json".to_owned();
    let observation = RequestContextObservationReference::new(
        "provider-round".to_owned(),
        1,
        6,
        reference.clone(),
    )
    .unwrap();
    let mut source = event(&request, reference.clone());
    let SessionEventKind::TraceEvent { trace } = &mut source.kind else {
        panic!("trace");
    };
    trace.turn_id = Some("turn".to_owned());
    trace.payload = SessionTracePayload::ProviderEvent {
        event_type: REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE.to_owned(),
        detail: Some(observation.encode().unwrap()),
    };
    assert_eq!(retained_reference(&source, &request), Ok(reference));
    for sequence in [7, 8] {
        let mut future = observation.clone();
        future.model_request_sequence = sequence;
        let SessionEventKind::TraceEvent { trace } = &mut source.kind else {
            panic!("trace");
        };
        trace.payload = SessionTracePayload::ProviderEvent {
            event_type: REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE.to_owned(),
            detail: Some(future.encode().unwrap()),
        };
        assert_eq!(
            retained_reference(&source, &request),
            Err(ReadTraceBlobError::Damaged)
        );
    }
    let SessionEventKind::TraceEvent { trace } = &mut source.kind else {
        panic!("trace");
    };
    trace.turn_id = None;
    trace.payload = SessionTracePayload::ProviderEvent {
        event_type: REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE.to_owned(),
        detail: Some(observation.encode().unwrap()),
    };
    assert_eq!(
        retained_reference(&source, &request),
        Err(ReadTraceBlobError::Damaged)
    );
}

#[test]
fn malformed_future_and_nonreference_observations_are_not_exportable() {
    use bcode_session_models::{
        REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE, RequestContextObservationReference,
    };
    let (_root, request, mut reference) = fixture(b"payload");
    reference.content_type = "application/json".to_owned();
    let observation =
        RequestContextObservationReference::new("round".to_owned(), 1, 6, reference.clone())
            .unwrap();
    let mut value = serde_json::to_value(&observation).unwrap();
    value["version"] = serde_json::json!(2);
    for (event_type, detail, expected) in [
        (
            REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE,
            "{broken".to_owned(),
            ReadTraceBlobError::Unsupported,
        ),
        (
            REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE,
            value.to_string(),
            ReadTraceBlobError::Unsupported,
        ),
        (
            "request_projection",
            observation.encode().unwrap(),
            ReadTraceBlobError::NotFound,
        ),
    ] {
        let mut source = event(&request, reference.clone());
        let SessionEventKind::TraceEvent { trace } = &mut source.kind else {
            panic!("trace");
        };
        trace.turn_id = Some("turn".to_owned());
        trace.payload = SessionTracePayload::ProviderEvent {
            event_type: event_type.to_owned(),
            detail: Some(detail),
        };
        assert_eq!(retained_reference(&source, &request), Err(expected));
    }
    reference.path = "../private.json".to_owned();
    let foreign =
        RequestContextObservationReference::new("round".to_owned(), 1, 6, reference).unwrap();
    assert_eq!(
        validate_reference(&request, &foreign.blob),
        Err(ReadTraceBlobError::Damaged)
    );
}

#[test]
fn canonical_event_identity_and_version_are_required() {
    let (_root, request, reference) = fixture(b"payload");
    let mut source = event(&request, reference.clone());
    assert_eq!(retained_reference(&source, &request), Ok(reference.clone()));
    source.session_id = SessionId::new();
    assert_eq!(
        retained_reference(&source, &request),
        Err(ReadTraceBlobError::Damaged)
    );
    source.session_id = request.session_id;
    source.sequence += 1;
    assert_eq!(
        retained_reference(&source, &request),
        Err(ReadTraceBlobError::Damaged)
    );
    source.sequence = request.sequence;
    source.schema_version += 1;
    assert_eq!(
        retained_reference(&source, &request),
        Err(ReadTraceBlobError::Unsupported)
    );
    source.schema_version = CURRENT_SESSION_EVENT_SCHEMA_VERSION;
    let SessionEventKind::TraceEvent { trace } = &mut source.kind else {
        panic!("trace fixture");
    };
    trace.payload = SessionTracePayload::ToolInvocationStarted {
        tool_call_id: "call".to_string(),
        plugin_id: "tool".to_string(),
        tool_name: "read".to_string(),
        side_effect: "read".to_string(),
        requires_permission: false,
        arguments: Some(reference.clone()),
    };
    assert_eq!(retained_reference(&source, &request), Ok(reference));
    let SessionEventKind::TraceEvent { trace } = &mut source.kind else {
        panic!("trace fixture");
    };
    trace.payload = SessionTracePayload::ToolInvocationStarted {
        tool_call_id: "call".to_string(),
        plugin_id: "tool".to_string(),
        tool_name: "read".to_string(),
        side_effect: "read".to_string(),
        requires_permission: false,
        arguments: None,
    };
    assert_eq!(
        retained_reference(&source, &request),
        Err(ReadTraceBlobError::NotFound)
    );
}

#[test]
fn unsupported_and_invalid_requests_fail_closed() {
    let mut request = request(SessionId::new());
    request.version += 1;
    assert_eq!(
        validate_request(&request),
        Err(ReadTraceBlobError::Unsupported)
    );
    request.version = SESSION_TRACE_EXPORT_VERSION;
    for length in [0, MAX_SESSION_TRACE_BLOB_BYTES + 1, u32::MAX] {
        request.length = length;
        assert_eq!(
            validate_request(&request),
            Err(ReadTraceBlobError::InvalidRange)
        );
    }
    request.length = 1;
    request.offset = u64::MAX;
    assert_eq!(
        validate_request(&request),
        Err(ReadTraceBlobError::InvalidRange)
    );
}

#[test]
fn traversal_and_cross_session_references_are_rejected() {
    let (_root, request, reference) = fixture(b"payload");
    for path in [
        format!("../{}", reference.path),
        format!("/{}", reference.path),
        format!("{}/blobs/../{}", request.session_id, reference.path),
        reference.path.replace("/blobs/", "/blobs/./"),
        reference.path.replace('/', "\\"),
        reference.path.replace(
            &request.session_id.to_string(),
            &SessionId::new().to_string(),
        ),
    ] {
        let mut invalid = reference.clone();
        invalid.path = path;
        assert_eq!(
            validate_reference(&request, &invalid),
            Err(ReadTraceBlobError::Damaged)
        );
    }
}

#[test]
fn unsupported_metadata_and_oversized_captures_are_explicit() {
    let (_root, mut request, mut reference) = fixture(b"payload");
    reference.content_type = "application/future".to_string();
    assert_eq!(
        validate_reference(&request, &reference),
        Err(ReadTraceBlobError::Unsupported)
    );
    reference.content_type = "text/plain".to_string();
    reference.byte_len = u64::from(MAX_SESSION_TRACE_BLOB_BYTES) + 1;
    assert_eq!(
        validate_reference(&request, &reference),
        Err(ReadTraceBlobError::TooLarge)
    );
    reference.byte_len = 7;
    request.offset = 8;
    assert_eq!(
        validate_reference(&request, &reference),
        Err(ReadTraceBlobError::InvalidRange)
    );
}

#[cfg(unix)]
#[test]
fn partial_ranges_verify_entire_blob_and_preserve_capture_status() {
    let (root, mut request, mut reference) = fixture(b"abcdefg");
    request.offset = 2;
    for completeness in [
        TraceBlobCompleteness::Complete,
        TraceBlobCompleteness::Retained,
        TraceBlobCompleteness::Truncated,
    ] {
        reference.completeness = completeness;
        reference.redaction = TraceRedaction::ManualRequired;
        let range = read_verified(root.path(), &request, &reference).expect("verified range");
        assert_eq!(range.bytes, b"cde");
        assert_eq!(range.completeness, completeness);
        assert_eq!(range.redaction, TraceRedaction::ManualRequired);
        assert_eq!(range.byte_len, 7);
        assert!(!range.eof);
    }
    request.offset = 6;
    let range = read_verified(root.path(), &request, &reference).expect("last byte");
    assert_eq!(range.bytes, b"g");
    assert!(range.eof);
    request.offset = 7;
    let range = read_verified(root.path(), &request, &reference).expect("EOF");
    assert!(range.bytes.is_empty());
    assert!(range.eof);
    // Corrupt bytes outside the requested range still invalidate the response.
    request.offset = 0;
    std::fs::write(root.path().join(&reference.path), b"abcdefX").expect("corrupt tail");
    assert_eq!(
        read_verified(root.path(), &request, &reference),
        Err(ReadTraceBlobError::Damaged)
    );
}

#[cfg(unix)]
#[test]
fn missing_truncated_and_extended_content_are_distinct_from_verified_bytes() {
    let (root, request, reference) = fixture(b"payload");
    let path = root.path().join(&reference.path);
    for bytes in [b"pay".as_slice(), b"payload-extra".as_slice()] {
        std::fs::write(&path, bytes).expect("change length");
        assert_eq!(
            read_verified(root.path(), &request, &reference),
            Err(ReadTraceBlobError::Damaged)
        );
    }
    std::fs::remove_file(&path).expect("remove fixture blob");
    assert_eq!(
        read_verified(root.path(), &request, &reference),
        Err(ReadTraceBlobError::Missing)
    );
}

#[cfg(unix)]
#[test]
fn symlinks_at_each_untrusted_component_are_rejected() {
    use std::os::unix::fs::symlink;
    // Even a symlink to identical valid bytes is not authority to read another root.
    for depth in 0..3 {
        let (root, request, reference) = fixture(b"payload");
        let outside = tempfile::tempdir().expect("outside root");
        let parts: Vec<_> = reference.path.split('/').collect();
        let relative: std::path::PathBuf = parts[..=depth].iter().collect();
        let path = root.path().join(&relative);
        let target = outside.path().join("moved");
        std::fs::rename(&path, &target).expect("move fixture component");
        symlink(&target, &path).expect("symlink fixture component");
        assert_eq!(
            read_verified(root.path(), &request, &reference),
            Err(ReadTraceBlobError::Damaged)
        );
    }
}

#[cfg(unix)]
#[test]
fn zero_byte_capture_and_disabled_store_are_explicit() {
    let (root, request, reference) = fixture(b"");
    let range = read_verified(root.path(), &request, &reference).expect("empty retained capture");
    assert!(range.bytes.is_empty());
    assert!(range.eof);
    assert_eq!(range.byte_len, 0);
    assert_eq!(
        read_verified(Path::new(""), &request, &reference),
        Err(ReadTraceBlobError::Unavailable)
    );
}
