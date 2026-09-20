//! Provider-owned conversion of web conversation graphs into selected-branch snapshots.
//!
//! No network access, credential handling, or canonical persistence occurs here.

/// Bounded provider-owned remote history access.
pub mod client;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One normalized historical message. Text is untrusted source content, never instructions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryMessage {
    /// Stable source node identity, scoped by the conversation and remote account.
    pub node_id: String,
    /// Original role; callers must not elevate system/developer messages into local instructions.
    pub role: String,
    /// Source timestamp in seconds, if supplied and valid.
    pub created_at: Option<f64>,
    /// Source text only; no attachment URLs or fabricated tool executions.
    pub text: String,
}

/// Explicit losses or unsupported source features in a selected-branch snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryFidelityWarning {
    /// Nodes outside the selected ancestry are not included in this snapshot.
    AlternateNodes,
    /// An image/file or structured content part was not downloaded.
    AttachmentNotImported,
    /// The content type cannot yet be represented faithfully.
    UnsupportedContent,
    /// A nonstandard role needs historical presentation rather than executable interpretation.
    UnsupportedRole,
    /// Metadata indicates source attachments without a durable imported asset.
    AttachmentMetadata,
    /// Timestamp was malformed; no replacement timestamp was invented.
    InvalidTimestamp,
}

/// A complete selected ancestry, not a merged transcript of alternate answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistorySnapshot {
    /// API identity; never derived from the website's potentially different URL identity.
    pub conversation_id: String,
    /// Source title.
    pub title: Option<String>,
    /// Selected leaf identity.
    pub selected_node: String,
    /// Messages in parent-to-child order, regardless of timestamp ordering.
    pub messages: Vec<HistoryMessage>,
    /// Visible fidelity limitations, deduplicated deterministically.
    pub warnings: BTreeSet<HistoryFidelityWarning>,
}

/// Secret-safe conversion failure. No source payload or identifiers are interpolated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryDecodeError {
    /// Payload exceeds the caller's response budget.
    TooLarge,
    /// Invalid JSON or unsupported required structure.
    InvalidSchema,
    /// Returned conversation identity does not match the requested identity.
    IdentityMismatch,
    /// Selected ancestry references a missing node or disagrees with embedded identity.
    InvalidGraph,
    /// Selected ancestry contains a cycle.
    Cycle,
}

#[derive(Deserialize)]
struct WireHistory {
    conversation_id: String,
    title: Option<String>,
    current_node: String,
    mapping: BTreeMap<String, Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    parent: Option<String>,
    message: Option<Value>,
}

/// Decode a bounded response and follow only its selected parent chain.
///
/// # Errors
/// Returns a normalized error for excess bytes, malformed schema, identity mismatch,
/// missing ancestry, inconsistent node IDs, or cycles. No partial snapshot is returned.
pub fn decode_history(
    bytes: &[u8],
    expected_id: &str,
    max_bytes: usize,
) -> Result<HistorySnapshot, HistoryDecodeError> {
    if bytes.len() > max_bytes {
        return Err(HistoryDecodeError::TooLarge);
    }
    let source: WireHistory =
        serde_json::from_slice(bytes).map_err(|_| HistoryDecodeError::InvalidSchema)?;
    if source.conversation_id != expected_id || expected_id.is_empty() {
        return Err(HistoryDecodeError::IdentityMismatch);
    }
    let mut visited = BTreeSet::new();
    let mut ancestry = Vec::new();
    let mut current = Some(source.current_node.as_str());
    while let Some(id) = current {
        if !visited.insert(id) {
            return Err(HistoryDecodeError::Cycle);
        }
        let node = source
            .mapping
            .get(id)
            .ok_or(HistoryDecodeError::InvalidGraph)?;
        if node.id != id {
            return Err(HistoryDecodeError::InvalidGraph);
        }
        ancestry.push(node);
        current = node.parent.as_deref();
    }
    let mut warnings = BTreeSet::new();
    if visited.len() < source.mapping.len() {
        warnings.insert(HistoryFidelityWarning::AlternateNodes);
    }
    let messages = ancestry
        .into_iter()
        .rev()
        .filter_map(|node| node.message.as_ref().map(|message| (node, message)))
        .map(|(node, message)| decode_message(&node.id, message, &mut warnings))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(HistorySnapshot {
        conversation_id: source.conversation_id,
        title: source.title,
        selected_node: source.current_node,
        messages,
        warnings,
    })
}

fn decode_message(
    id: &str,
    message: &Value,
    warnings: &mut BTreeSet<HistoryFidelityWarning>,
) -> Result<HistoryMessage, HistoryDecodeError> {
    let role = message
        .pointer("/author/role")
        .and_then(Value::as_str)
        .ok_or(HistoryDecodeError::InvalidSchema)?;
    if !matches!(role, "user" | "assistant" | "system" | "developer" | "tool") {
        warnings.insert(HistoryFidelityWarning::UnsupportedRole);
    }
    let timestamp = message.get("create_time");
    let created_at = timestamp
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0);
    if timestamp.is_some_and(|v| !v.is_null()) && created_at.is_none() {
        warnings.insert(HistoryFidelityWarning::InvalidTimestamp);
    }
    if message
        .pointer("/metadata/attachments")
        .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
    {
        warnings.insert(HistoryFidelityWarning::AttachmentMetadata);
    }
    let mut text = Vec::new();
    match message
        .pointer("/content/content_type")
        .and_then(Value::as_str)
    {
        Some("text" | "multimodal_text") => {
            let parts = message
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .ok_or(HistoryDecodeError::InvalidSchema)?;
            for part in parts {
                if let Some(value) = part.as_str() {
                    text.push(value);
                } else {
                    warnings.insert(HistoryFidelityWarning::AttachmentNotImported);
                }
            }
        }
        _ => {
            warnings.insert(HistoryFidelityWarning::UnsupportedContent);
        }
    }
    Ok(HistoryMessage {
        node_id: id.to_owned(),
        role: role.to_owned(),
        created_at,
        text: text.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn graph() -> Value {
        json!({"conversation_id":"api-id", "title":"Synthetic", "current_node":"answer", "mapping": {
            "root":{"id":"root", "parent":null, "message":null},
            "prompt":{"id":"prompt", "parent":"root", "message":{"author":{"role":"user"}, "create_time":20, "content":{"content_type":"text", "parts":["question"]}}},
            "answer":{"id":"answer", "parent":"prompt", "message":{"author":{"role":"assistant"}, "create_time":10, "content":{"content_type":"text", "parts":["selected answer"]}}},
            "alternative":{"id":"alternative", "parent":"prompt", "message":{"author":{"role":"assistant"}, "content":{"content_type":"text", "parts":["other answer"]}}}
        }})
    }

    fn decode(value: &Value) -> Result<HistorySnapshot, HistoryDecodeError> {
        decode_history(&serde_json::to_vec(value).unwrap(), "api-id", 65536)
    }

    #[test]
    fn selected_ancestry_not_timestamp_order() {
        let result = decode(&graph()).unwrap();
        assert_eq!(
            result
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            ["question", "selected answer"]
        );
        assert!(
            result
                .warnings
                .contains(&HistoryFidelityWarning::AlternateNodes)
        );
    }

    #[test]
    fn broken_graphs_never_return_partial_history() {
        let mut value = graph();
        value["mapping"]["prompt"]["parent"] = json!("answer");
        assert_eq!(decode(&value), Err(HistoryDecodeError::Cycle));
        value["mapping"]["prompt"]["parent"] = json!("missing");
        assert_eq!(decode(&value), Err(HistoryDecodeError::InvalidGraph));
        value = graph();
        value["mapping"]["answer"]["id"] = json!("wrong");
        assert_eq!(decode(&value), Err(HistoryDecodeError::InvalidGraph));
    }

    #[test]
    fn identity_and_size_are_checked() {
        let bytes = serde_json::to_vec(&graph()).unwrap();
        assert_eq!(
            decode_history(&bytes, "WEB:api-id", bytes.len()),
            Err(HistoryDecodeError::IdentityMismatch)
        );
        assert_eq!(
            decode_history(&bytes, "api-id", bytes.len() - 1),
            Err(HistoryDecodeError::TooLarge)
        );
        assert_eq!(
            decode_history(b"{}", "api-id", 100),
            Err(HistoryDecodeError::InvalidSchema)
        );
    }

    #[test]
    fn attachment_and_unknown_content_are_visible_without_leaking_urls() {
        let mut value = graph();
        value["mapping"]["answer"]["message"]["content"] = json!({"content_type":"multimodal_text", "parts":["caption", {"asset_pointer":"secret-url"}]});
        let result = decode(&value).unwrap();
        assert_eq!(result.messages[1].text, "caption");
        assert!(
            result
                .warnings
                .contains(&HistoryFidelityWarning::AttachmentNotImported)
        );
        value["mapping"]["answer"]["message"]["content"] =
            json!({"content_type":"unknown", "secret":"hidden"});
        assert!(
            decode(&value)
                .unwrap()
                .warnings
                .contains(&HistoryFidelityWarning::UnsupportedContent)
        );
    }
}
