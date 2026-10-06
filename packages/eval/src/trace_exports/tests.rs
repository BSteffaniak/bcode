use super::*;
use bcode_session_models::{
    SessionId, SessionTraceBlobRange, TraceBlobCompleteness, TraceBlobRef, TraceRedaction,
};
use sha2::{Digest as _, Sha256};

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
        let end = (offset + 32768).min(bytes.len());
        Ok(serde_json::to_value(SessionTraceBlobRange {
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
        .unwrap())
    };
    assert_eq!(read_cli_capture(&capture, &read).unwrap().bytes, bytes);
    let corrupt = |args: &[String], label: &str, duration| {
        let mut value = read(args, label, duration)?;
        value["bytes"][0] = json!(0);
        Ok(value)
    };
    assert!(read_cli_capture(&capture, &corrupt).is_none());
}
