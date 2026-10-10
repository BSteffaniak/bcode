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
