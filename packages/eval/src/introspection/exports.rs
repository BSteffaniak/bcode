//! Receipt integration is pure and trusts only the coordinator's verified export outcome.
use bcode_session_models::{SessionEvent, TraceBlobCompleteness};
use serde_json::{Value, json};

pub(super) fn apply(events: &[SessionEvent], document: &Value) -> Value {
    let mut summary = super::summary(events);
    let valid_schema = document["schema"] == "bcode.eval.trace_exports.v1";
    let receipts = document["receipts"].as_array().filter(|_| valid_schema);
    let references = super::referenced_blobs(events);
    let mut outcomes = Vec::new();
    let mut complete_requests = 0;
    for reference in &references {
        let matching: Vec<_> = receipts
            .into_iter()
            .flatten()
            .filter(|receipt| {
                receipt["session_id"] == json!(reference.session_id)
                    && receipt["sequence"] == json!(reference.sequence)
            })
            .collect();
        let (status, complete) = match matching.as_slice() {
            [receipt] if receipt["status"] == "exported" => {
                let matches_reference = receipt["sha256"] == json!(reference.blob.sha256)
                    && receipt["byte_len"] == json!(reference.blob.byte_len)
                    && receipt["completeness"] == json!(reference.blob.completeness)
                    && receipt["redaction"] == json!(reference.blob.redaction)
                    && reference.blob.sha256.len() == 64
                    && reference
                        .blob
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit());
                if matches_reference {
                    (
                        "exported",
                        reference.blob.completeness == TraceBlobCompleteness::Complete,
                    )
                } else {
                    ("verification_failed", false)
                }
            }
            [receipt] if receipt["status"] == "unavailable" => ("unavailable", false),
            [receipt] if receipt["status"] == "verification_failed" => {
                ("verification_failed", false)
            }
            [] => (
                if valid_schema {
                    "missing_receipt"
                } else {
                    "unsupported_receipt_schema"
                },
                false,
            ),
            [_] => ("unsupported_receipt_status", false),
            _ => ("conflicting_receipts", false),
        };
        if reference.kind == "model_request" && status == "exported" && complete {
            complete_requests += 1;
        }
        outcomes.push(
            json!({"session_id":reference.session_id,"sequence":reference.sequence,
            "turn_id":reference.turn_id,"tool_call_id":reference.tool_call_id,
            "kind":reference.kind,"status":status,"complete":complete}),
        );
    }
    let requests = summary["model_requests_observed"]
        .as_u64()
        .unwrap_or_default();
    summary["export_receipt_schema_supported"] = json!(valid_schema && receipts.is_some());
    summary["export_outcomes"] = json!(outcomes);
    summary["complete_model_inputs_exported"] =
        json!(requests > 0 && complete_requests == requests);
    summary["export_scope"] = json!(
        "retained request-build observations only; redacted content and unobserved requests excluded"
    );
    summary
}
