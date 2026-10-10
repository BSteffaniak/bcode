//! Explicit sensitive trace capture export through the public application boundary.
use bcode_client::BcodeClient;
use bcode_session_models::{
    MAX_SESSION_TRACE_BLOB_BYTES, ReadSessionTraceBlobRequest, SESSION_TRACE_EXPORT_VERSION,
    SessionEvent,
};
use serde_json::json;
use std::path::Path;

#[cfg(test)]
mod tests;

/// Prioritize model inputs across all observed sessions before optional tool I/O.
/// Receipts remain session-scoped and unavailable boundaries are retained explicitly.
///
/// # Errors
/// Returns an error if retained bytes or receipts cannot be written.
pub fn export_cli_batch(
    sessions: &[(std::path::PathBuf, Vec<SessionEvent>)],
    invoke: &impl Fn(
        &[String],
        &str,
        std::time::Duration,
    ) -> Result<serde_json::Value, super::EvalError>,
) -> Result<(), super::EvalError> {
    let mut receipts = vec![Vec::new(); sessions.len()];
    let mut captures = sessions
        .iter()
        .enumerate()
        .flat_map(|(index, (_, events))| {
            super::introspection::referenced_blobs(events)
                .into_iter()
                .map(move |capture| (index, capture))
        })
        .collect::<Vec<_>>();
    captures.sort_by_key(|(_, capture)| capture.kind != "model_request");
    let mut outcomes: std::collections::BTreeMap<
        _,
        (serde_json::Value, Option<std::path::PathBuf>),
    > = std::collections::BTreeMap::new();
    for (index, capture) in captures {
        let root = sessions[index].0.with_extension("captures");
        let key = (
            capture.session_id,
            capture.sequence,
            serde_json::to_string(&capture.blob)?,
        );
        let label = format!(
            "capture-{index}-{}-{}",
            capture.session_id, capture.sequence
        );
        let (receipt, source) = if let Some((receipt, source)) = outcomes.get(&key) {
            (receipt.clone(), source.clone())
        } else {
            let (receipt, source) = match read_cli_capture(&capture, &label, invoke) {
                Ok(range) => {
                    std::fs::create_dir_all(&root)?;
                    let name = format!(
                        "{}-{}-{}.blob",
                        capture.session_id, capture.sequence, range.sha256
                    );
                    let source = root.join(&name);
                    std::fs::write(&source, &range.bytes)?;
                    (
                        json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"exported","file":name,"sha256":range.sha256,"byte_len":range.byte_len,"completeness":range.completeness,"redaction":range.redaction,"observation":label}),
                        Some(source),
                    )
                }
                Err(failure) => (
                    json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"unavailable","failure":failure,"observation":label}),
                    None,
                ),
            };
            outcomes.insert(key, (receipt.clone(), source.clone()));
            (receipt, source)
        };
        if let Some(source) = source {
            let destination = root.join(receipt["file"].as_str().expect("exported filename"));
            if source != destination {
                // Reuse only exact, fully verified bytes, never an earlier partial range.
                use sha2::{Digest as _, Sha256};
                use std::io::Read as _;
                let mut bytes = Vec::new();
                std::fs::File::open(&source)?
                    .take(capture.blob.byte_len + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 != capture.blob.byte_len
                    || hex::encode(Sha256::digest(&bytes)) != capture.blob.sha256
                {
                    receipts[index].push(json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"unavailable","failure":CaptureFailure::Integrity,"observation":receipt["observation"]}));
                    continue;
                }
                std::fs::create_dir_all(&root)?;
                std::fs::write(destination, bytes)?;
            }
        }
        receipts[index].push(receipt);
    }
    for ((path, _), receipts) in sessions.iter().zip(receipts) {
        super::write_json(
            path.with_extension("captures.json"),
            &json!({"schema":"bcode.eval.trace_exports.v1","sensitive":true,"receipts":receipts}),
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum CaptureFailure {
    Deadline,
    Invocation,
    Decode,
    Integrity,
}

impl CaptureFailure {
    fn invocation(error: &super::EvalError) -> Self {
        // Exact local capture-budget sentinel, never provider or tool display text.
        if matches!(error, super::EvalError::Validation(message)
            if message == "post-execution capture allowance exhausted")
        {
            Self::Deadline
        } else if matches!(error, super::EvalError::Json(_)) {
            Self::Decode
        } else {
            Self::Invocation
        }
    }
}

fn read_cli_capture(
    capture: &super::introspection::CaptureReference,
    label: &str,
    invoke: &impl Fn(
        &[String],
        &str,
        std::time::Duration,
    ) -> Result<serde_json::Value, super::EvalError>,
) -> Result<bcode_session_models::SessionTraceBlobRange, CaptureFailure> {
    use sha2::{Digest as _, Sha256};
    // Pretty-printed byte arrays expand on the CLI wire. Keep each response below
    // the subprocess reader's budget independently of the retained blob budget.
    const CHUNK: u32 = 64 * 1024;
    if capture.blob.byte_len > u64::from(MAX_SESSION_TRACE_BLOB_BYTES) {
        return Err(CaptureFailure::Integrity);
    }
    let mut bytes = Vec::new();
    loop {
        let offset = bytes.len() as u64;
        let value = invoke(
            &[
                "session".into(),
                "trace-range".into(),
                capture.session_id.to_string(),
                capture.sequence.to_string(),
                "--offset".into(),
                offset.to_string(),
                "--length".into(),
                CHUNK.to_string(),
            ],
            &format!("{label}-{offset}"),
            std::time::Duration::from_secs(15),
        )
        .map_err(|error| CaptureFailure::invocation(&error))?;
        let mut range: bcode_session_models::SessionTraceBlobRange =
            serde_json::from_value(value).map_err(|_| CaptureFailure::Decode)?;
        if range.version != SESSION_TRACE_EXPORT_VERSION
            || range.session_id != capture.session_id
            || range.sequence != capture.sequence
            || range.sha256 != capture.blob.sha256
            || range.content_type != capture.blob.content_type
            || range.byte_len != capture.blob.byte_len
            || range.completeness != capture.blob.completeness
            || range.redaction != capture.blob.redaction
            || range.offset != offset
            || range.bytes.len() > CHUNK as usize
        {
            return Err(CaptureFailure::Integrity);
        }
        let end = offset
            .checked_add(range.bytes.len() as u64)
            .ok_or(CaptureFailure::Integrity)?;
        if end > range.byte_len || range.eof != (end == range.byte_len) {
            return Err(CaptureFailure::Integrity);
        }
        bytes.extend_from_slice(&range.bytes);
        if range.eof {
            if hex::encode(Sha256::digest(&bytes)) != capture.blob.sha256 {
                return Err(CaptureFailure::Integrity);
            }
            range.offset = 0;
            range.bytes = bytes;
            return Ok(range);
        }
        if end == offset {
            return Err(CaptureFailure::Integrity);
        }
    }
}

pub async fn export(
    client: &BcodeClient,
    path: &Path,
    events: &[SessionEvent],
) -> Result<(), super::EvalError> {
    use sha2::{Digest as _, Sha256};
    let root = path.with_extension("captures");
    let mut receipts = Vec::new();
    for capture in super::introspection::referenced_blobs(events) {
        let reference = &capture.blob;
        let request = ReadSessionTraceBlobRequest {
            version: SESSION_TRACE_EXPORT_VERSION,
            session_id: capture.session_id,
            sequence: capture.sequence,
            offset: 0,
            length: MAX_SESSION_TRACE_BLOB_BYTES,
        };
        match client.session_trace_blob_range(request).await {
            Ok(range) if range.version == SESSION_TRACE_EXPORT_VERSION
                && range.session_id == capture.session_id && range.sequence == capture.sequence
                && range.offset == 0 && range.sha256 == reference.sha256
                && range.byte_len == reference.byte_len
                && range.completeness == reference.completeness && range.redaction == reference.redaction
                && range.eof && range.bytes.len() as u64 == range.byte_len
                && hex::encode(Sha256::digest(&range.bytes)) == reference.sha256 => {
                std::fs::create_dir_all(&root)?;
                let name = format!("{}-{}.blob", capture.session_id, capture.sequence);
                std::fs::write(root.join(&name), &range.bytes)?;
                receipts.push(json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"exported","file":name,"sha256":range.sha256,"byte_len":range.byte_len,"completeness":range.completeness,"redaction":range.redaction}));
            }
            Ok(_) => receipts.push(json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"verification_failed"})),
            Err(_) => receipts.push(json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"unavailable"})),
        }
    }
    super::write_json(
        path.with_extension("captures.json"),
        &json!({"schema":"bcode.eval.trace_exports.v1","sensitive":true,"receipts":receipts}),
    )
}
