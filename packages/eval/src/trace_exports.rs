//! Explicit sensitive trace capture export through the public application boundary.
use bcode_client::BcodeClient;
use bcode_session_models::{
    MAX_SESSION_TRACE_BLOB_BYTES, ReadSessionTraceBlobRequest, SESSION_TRACE_EXPORT_VERSION,
    SessionEvent, SessionEventKind, SessionTracePayload,
};
use serde_json::json;
use std::path::Path;

pub fn export_cli(
    path: &Path,
    events: &[SessionEvent],
    invoke: &impl Fn(
        &[String],
        &str,
        std::time::Duration,
    ) -> Result<serde_json::Value, super::EvalError>,
) -> Result<(), super::EvalError> {
    let root = path.with_extension("captures");
    let mut receipts = Vec::new();
    for capture in super::introspection::referenced_blobs(events) {
        let result = invoke(
            &[
                "session".into(),
                "trace-range".into(),
                capture.session_id.to_string(),
                capture.sequence.to_string(),
                "--length".into(),
                MAX_SESSION_TRACE_BLOB_BYTES.to_string(),
            ],
            &format!("capture-{}-{}", capture.session_id, capture.sequence),
            std::time::Duration::from_secs(15),
        );
        let range = result.ok().and_then(|value| {
            serde_json::from_value::<bcode_session_models::SessionTraceBlobRange>(value).ok()
        });
        if let Some(range) = range.filter(|range| {
            range.version == SESSION_TRACE_EXPORT_VERSION
                && range.session_id == capture.session_id
                && range.sequence == capture.sequence
                && range.sha256 == capture.blob.sha256
                && range.byte_len == capture.blob.byte_len
                && range.offset == 0
                && range.eof
                && range.bytes.len() as u64 == range.byte_len
        }) {
            std::fs::create_dir_all(&root)?;
            let name = format!("{}-{}.blob", capture.session_id, capture.sequence);
            std::fs::write(root.join(&name), &range.bytes)?;
            receipts.push(json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"exported","file":name,"sha256":range.sha256,"byte_len":range.byte_len,"completeness":range.completeness,"redaction":range.redaction}));
        } else {
            receipts.push(json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"unavailable"}));
        }
    }
    super::write_json(
        path.with_extension("captures.json"),
        &json!({"schema":"bcode.eval.trace_exports.v1","sensitive":true,"receipts":receipts}),
    )
}

pub async fn export(
    client: &BcodeClient,
    path: &Path,
    events: &[SessionEvent],
) -> Result<(), super::EvalError> {
    let root = path.with_extension("captures");
    let mut receipts = Vec::new();
    for event in events {
        let SessionEventKind::TraceEvent { trace } = &event.kind else {
            continue;
        };
        let reference = match &trace.payload {
            SessionTracePayload::ModelRequestBuilt { request, .. } => request.as_ref(),
            SessionTracePayload::ToolInvocationStarted { arguments, .. } => arguments.as_ref(),
            SessionTracePayload::ToolInvocationFinished { output, .. } => output.as_ref(),
            _ => None,
        };
        let Some(reference) = reference else { continue };
        let request = ReadSessionTraceBlobRequest {
            version: SESSION_TRACE_EXPORT_VERSION,
            session_id: event.session_id,
            sequence: event.sequence,
            offset: 0,
            length: MAX_SESSION_TRACE_BLOB_BYTES,
        };
        match client.session_trace_blob_range(request).await {
            Ok(range) if range.sha256 == reference.sha256 && range.byte_len == reference.byte_len && range.eof && range.bytes.len() as u64 == range.byte_len => {
                std::fs::create_dir_all(&root)?;
                let name = format!("{}-{}.blob", event.session_id, event.sequence);
                std::fs::write(root.join(&name), &range.bytes)?;
                receipts.push(json!({"session_id":event.session_id,"sequence":event.sequence,"status":"exported","file":name,"sha256":range.sha256,"byte_len":range.byte_len,"completeness":range.completeness,"redaction":range.redaction}));
            }
            Ok(_) => receipts.push(json!({"session_id":event.session_id,"sequence":event.sequence,"status":"verification_failed"})),
            Err(_) => receipts.push(json!({"session_id":event.session_id,"sequence":event.sequence,"status":"unavailable"})),
        }
    }
    super::write_json(
        path.with_extension("captures.json"),
        &json!({"schema":"bcode.eval.trace_exports.v1","sensitive":true,"receipts":receipts}),
    )
}
