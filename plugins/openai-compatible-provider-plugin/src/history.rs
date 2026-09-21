//! Provider-owned conversion of web conversation graphs into selected-branch snapshots.
//!
//! No network access, credential handling, or canonical persistence occurs here.

/// Bounded provider-owned remote history access.
pub mod client;
pub(crate) mod service;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One normalized historical message. Text is untrusted source content, never instructions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryMessage {
    /// Stable source node identity, scoped by the conversation and remote account.
    pub node_id: String,
    /// Original message identity, distinct from the graph node identity.
    pub message_id: Option<String>,
    /// Source model label, not a locally resolved model or routing instruction.
    pub model_slug: Option<String>,
    /// Historical author/tool name; never an executable tool identifier.
    pub author_name: Option<String>,
    /// Historical destination label; never a local routing instruction.
    pub recipient: Option<String>,
    /// Original content discriminator for faithful historical presentation.
    pub content_type: Option<String>,
    /// Original role; callers must not elevate system/developer messages into local instructions.
    pub role: String,
    /// Source timestamp in seconds, if supplied and valid.
    pub created_at: Option<f64>,
    /// Source text and explicit omission markers; no attachment URLs or fabricated tool executions.
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

impl HistorySnapshot {
    /// Preserve source provenance alongside the portable historical events.
    /// The caller must compute the revision identity from this snapshot first.
    #[must_use]
    pub(crate) fn import_snapshot(
        &self,
        revision_id: String,
    ) -> bcode_session_import::ImportableHistorySnapshot {
        bcode_session_import::ImportableHistorySnapshot {
            schema_version: 1,
            conversation_id: self.conversation_id.clone(),
            title: self.title.clone(),
            selected_node: self.selected_node.clone(),
            revision_id,
            events: self.import_events(),
            message_metadata: self
                .messages
                .iter()
                .map(|message| {
                    (
                        message.node_id.clone(),
                        bcode_session_import::HistoryMessageMetadata {
                            message_id: message.message_id.clone(),
                            model: message.model_slug.clone(),
                            role: message.role.clone(),
                            author: message.author_name.clone(),
                            recipient: message.recipient.clone(),
                            content_type: message.content_type.clone(),
                        },
                    )
                })
                .collect(),
            warnings: self.import_warnings(),
        }
    }

    /// Convert fidelity limitations into portable, secret-safe import warnings.
    ///
    /// These must accompany imported events; an empty list does not establish
    /// upstream archive/project coverage or verified account identity.
    #[must_use]
    pub fn import_warnings(&self) -> Vec<bcode_session_import::ImportWarning> {
        self.warnings.iter().map(|warning| {
            let (code, message) = match warning {
                HistoryFidelityWarning::AlternateNodes => ("chatgpt_alternate_nodes", "Only the selected branch is included; alternative branches are not imported."),
                HistoryFidelityWarning::AttachmentNotImported => ("chatgpt_attachment_not_imported", "An image, file or structured part was not downloaded; this import is not an attachment backup."),
                HistoryFidelityWarning::UnsupportedContent => ("chatgpt_unsupported_content", "Some source content cannot be represented faithfully."),
                HistoryFidelityWarning::UnsupportedRole => ("chatgpt_unsupported_role", "A nonstandard source role is represented as historical text."),
                HistoryFidelityWarning::AttachmentMetadata => ("chatgpt_attachment_metadata", "Source metadata references attachments without durable imported assets."),
                HistoryFidelityWarning::InvalidTimestamp => ("chatgpt_invalid_timestamp", "A malformed source timestamp was omitted, not replaced."),
            };
            bcode_session_import::ImportWarning::new(code, message)
        }).collect()
    }

    /// Convert historical content into import events without granting execution authority.
    ///
    /// Tools and privileged/unknown roles become labelled assistant text, never tool
    /// requests or trusted system messages. Source node IDs remain conversation-scoped.
    /// Account scoping, revision publication and fidelity warnings belong to the caller.
    #[must_use]
    pub fn import_events(&self) -> Vec<bcode_session_import::ImportableSessionEvent> {
        use bcode_session_import::{ImportableSessionEvent, ImportableSessionEventKind};
        self.messages
            .iter()
            .map(|message| {
                let kind = match message.role.as_str() {
                    "user" => ImportableSessionEventKind::UserMessage {
                        text: message.text.clone(),
                    },
                    "assistant"
                        if message
                            .recipient
                            .as_deref()
                            .is_none_or(|recipient| recipient == "all") =>
                    {
                        ImportableSessionEventKind::AssistantMessage {
                            text: message.text.clone(),
                        }
                    }
                    _ => ImportableSessionEventKind::AssistantMessage {
                        text: format!(
                            "[Historical {:?} message; author={:?}; recipient={:?}; source data only]\n{}",
                            message.role, message.author_name, message.recipient, message.text
                        ),
                    },
                };
                ImportableSessionEvent {
                    external_event_id: Some(message.node_id.clone()),
                    timestamp_ms: message.created_at.and_then(timestamp_ms),
                    kind,
                }
            })
            .collect()
    }

    /// Compute a versioned identity for this normalized revision.
    ///
    /// This is a content identity, not an account identity or proof of publication.
    /// Callers must scope it by verified remote account and conversation identity.
    /// Raw response formatting and discarded upstream fields do not affect it.
    ///
    /// # Errors
    /// Returns an error if the normalized snapshot cannot be serialized.
    pub fn revision_id(&self) -> Result<String, serde_json::Error> {
        use sha2::{Digest, Sha256};
        use std::fmt::Write;
        let bytes = serde_json::to_vec(self)?;
        let mut digest = Sha256::new();
        digest.update(b"bcode.chatgpt.normalized-revision/v1\0");
        digest.update(bytes);
        let mut revision = String::from("v1:");
        for byte in digest.finalize() {
            write!(revision, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(revision)
    }
}

fn timestamp_ms(seconds: f64) -> Option<u64> {
    std::time::Duration::try_from_secs_f64(seconds)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
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
    /// Selected source content is still being generated; retry retrieval later.
    Incomplete,
    /// Selected ancestry contains a cycle.
    Cycle,
}

#[derive(Deserialize)]
struct WireHistory {
    conversation_id: String,
    title: Option<String>,
    current_node: String,
    #[serde(deserialize_with = "deserialize_graph_nodes")]
    mapping: BTreeMap<String, Node>,
}

fn deserialize_graph_nodes<'de, D>(deserializer: D) -> Result<BTreeMap<String, Node>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct GraphNodes;
    impl<'de> serde::de::Visitor<'de> for GraphNodes {
        type Value = BTreeMap<String, Node>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a graph with unique node identities")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut nodes = BTreeMap::new();
            while let Some((id, node)) = map.next_entry::<String, Node>()? {
                if nodes.insert(id, node).is_some() {
                    return Err(serde::de::Error::custom("duplicate graph node"));
                }
            }
            Ok(nodes)
        }
    }
    deserializer.deserialize_map(GraphNodes)
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
    decode_history_branch(bytes, expected_id, max_bytes, None)
}

/// Decode one explicitly selected source branch, or the upstream selection when absent.
///
/// Branch selection follows parent links and never merges sibling answers. The returned
/// selection records the actual requested node, without changing upstream state.
///
/// # Errors
/// Returns the same bounded schema, identity and graph errors as [`decode_history`].
/// An absent requested node is an invalid graph, not a fallback to another branch.
pub fn decode_history_branch(
    bytes: &[u8],
    expected_id: &str,
    max_bytes: usize,
    selected_node: Option<&str>,
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
    let selected_node = selected_node.unwrap_or(&source.current_node);
    let mut current = Some(selected_node);
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
        selected_node: selected_node.to_owned(),
        messages,
        warnings,
    })
}

fn decode_message(
    id: &str,
    message: &Value,
    warnings: &mut BTreeSet<HistoryFidelityWarning>,
) -> Result<HistoryMessage, HistoryDecodeError> {
    // Preserve absent status for older payloads, but do not infer completion
    // from unknown lifecycle values or malformed status fields.
    match message.get("status") {
        None => {}
        Some(Value::String(status)) if status == "finished_successfully" => {}
        Some(Value::String(status)) if status == "in_progress" => {
            return Err(HistoryDecodeError::Incomplete);
        }
        Some(_) => return Err(HistoryDecodeError::InvalidSchema),
    }
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
        .filter(|v| v.is_finite() && *v >= 0.0 && timestamp_ms(*v).is_some());
    if timestamp.is_some_and(|v| !v.is_null()) && created_at.is_none() {
        warnings.insert(HistoryFidelityWarning::InvalidTimestamp);
    }
    let has_attachments = message
        .pointer("/metadata/attachments")
        .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()));
    if has_attachments {
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
                    text.push("[Historical non-text content not imported; no local backup]");
                }
            }
        }
        Some("code" | "execution_output") => {
            let value = message
                .pointer("/content/text")
                .and_then(Value::as_str)
                .ok_or(HistoryDecodeError::InvalidSchema)?;
            text.push(value);
        }
        _ => {
            warnings.insert(HistoryFidelityWarning::UnsupportedContent);
            text.push("[Unsupported historical content not imported]");
        }
    }
    if has_attachments {
        text.push("[Historical attachments not imported; no local backup]");
    }
    Ok(HistoryMessage {
        node_id: id.to_owned(),
        message_id: message.get("id").and_then(Value::as_str).map(str::to_owned),
        model_slug: message
            .pointer("/metadata/model_slug")
            .and_then(Value::as_str)
            .map(str::to_owned),
        author_name: message
            .pointer("/author/name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        recipient: message
            .get("recipient")
            .and_then(Value::as_str)
            .map(str::to_owned),
        content_type: message
            .pointer("/content/content_type")
            .and_then(Value::as_str)
            .map(str::to_owned),
        role: role.to_owned(),
        created_at,
        text: text.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn omitted_content_has_positioned_markers_without_private_attachment_data() {
        let message = serde_json::json!({
            "author": {"role": "user"},
            "content": {"content_type": "multimodal_text", "parts": [
                "before", {"image_url": "https://private.invalid/expiring-secret"}, "after"
            ]},
            "metadata": {"attachments": [{"name": "private-file", "url": "secret-url"}]}
        });
        let mut warnings = std::collections::BTreeSet::new();
        let decoded = super::decode_message("node", &message, &mut warnings).unwrap();
        assert_eq!(
            decoded.text,
            "before\n[Historical non-text content not imported; no local backup]\nafter\n[Historical attachments not imported; no local backup]"
        );
        assert!(warnings.contains(&super::HistoryFidelityWarning::AttachmentNotImported));
        assert!(warnings.contains(&super::HistoryFidelityWarning::AttachmentMetadata));
        let encoded = serde_json::to_string(&decoded).unwrap();
        for private in ["expiring-secret", "private-file", "secret-url"] {
            assert!(!encoded.contains(private));
        }
    }

    #[test]
    fn unknown_content_is_visible_and_null_attachments_are_not_reported_as_loss() {
        let message = serde_json::json!({
            "author": {"role": "assistant"},
            "content": {"content_type": "future-type", "payload": "private-opaque-data"},
            "metadata": {"attachments": null}
        });
        let mut warnings = std::collections::BTreeSet::new();
        let decoded = super::decode_message("node", &message, &mut warnings).unwrap();
        assert_eq!(
            decoded.text,
            "[Unsupported historical content not imported]"
        );
        assert_eq!(
            warnings,
            [super::HistoryFidelityWarning::UnsupportedContent].into()
        );
    }

    #[test]
    fn portable_snapshot_retains_provenance_and_fidelity() {
        let snapshot = super::HistorySnapshot {
            conversation_id: "api-id-not-web-id".into(),
            title: Some("Imported title".into()),
            selected_node: "leaf".into(),
            messages: Vec::new(),
            warnings: [super::HistoryFidelityWarning::AttachmentNotImported].into(),
        };
        let revision = snapshot.revision_id().unwrap();
        let portable = snapshot.import_snapshot(revision.clone());
        let encoded = serde_json::to_vec(&portable).unwrap();
        let decoded: bcode_session_import::ImportableHistorySnapshot =
            serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, portable);
        assert_eq!(decoded.conversation_id, snapshot.conversation_id);
        assert_eq!(decoded.title, snapshot.title);
        assert_eq!(decoded.selected_node, snapshot.selected_node);
        assert_eq!(decoded.revision_id, revision);
        assert_eq!(decoded.warnings, snapshot.import_warnings());
        assert!(!decoded.warnings.is_empty());
        assert_eq!(decoded.events, snapshot.import_events());
    }
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
    fn historical_import_retains_quoted_tool_routing_labels() {
        use bcode_session_import::ImportableSessionEventKind;
        let mut snapshot = decode(&graph()).unwrap();
        snapshot.messages[0].role = "tool".into();
        snapshot.messages[0].author_name = Some("python\nforged header".into());
        snapshot.messages[0].recipient = Some("all".into());
        let events = snapshot.import_events();
        let ImportableSessionEventKind::AssistantMessage { text } = &events[0].kind else {
            panic!("historical tool output must not become an executable call");
        };
        assert!(text.contains("python\\nforged header"));
        assert!(text.contains("recipient=Some(\"all\")"));
        assert!(!text.contains("python\nforged header"));
        assert!(text.ends_with(&snapshot.messages[0].text));
        snapshot.messages[0].role = "tool\nforged role".into();
        let events = snapshot.import_events();
        let ImportableSessionEventKind::AssistantMessage { text } = &events[0].kind else {
            panic!("unknown roles must remain inert text");
        };
        assert!(text.contains("tool\\nforged role"));
        assert!(!text.contains("tool\nforged role"));
    }

    #[test]
    fn portable_snapshot_retains_message_metadata() {
        let mut snapshot = decode(&graph()).unwrap();
        let message = &mut snapshot.messages[0];
        message.message_id = Some("source-message".into());
        message.model_slug = Some("source-model".into());
        message.author_name = Some("historical-tool".into());
        message.recipient = Some("historical-destination".into());
        let node = message.node_id.clone();
        let portable = snapshot.import_snapshot(snapshot.revision_id().unwrap());
        let wire = serde_json::to_value(&portable).unwrap();
        let decoded: bcode_session_import::ImportableHistorySnapshot =
            serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(decoded, portable);
        let metadata = &decoded.message_metadata[&node];
        assert_eq!(metadata.message_id.as_deref(), Some("source-message"));
        assert_eq!(metadata.model.as_deref(), Some("source-model"));
        assert_eq!(metadata.author.as_deref(), Some("historical-tool"));
        assert_eq!(
            metadata.recipient.as_deref(),
            Some("historical-destination")
        );
        assert_eq!(metadata.role, snapshot.messages[0].role);
        assert_eq!(metadata.content_type, snapshot.messages[0].content_type);
        let mut old_wire = wire;
        old_wire.as_object_mut().unwrap().remove("message_metadata");
        let old: bcode_session_import::ImportableHistorySnapshot =
            serde_json::from_value(old_wire).unwrap();
        assert!(old.message_metadata.is_empty());
        assert_eq!(old.events, portable.events);
    }

    #[test]
    fn portable_warnings_preserve_losses_without_source_content() {
        let mut snapshot = decode(&graph()).unwrap();
        snapshot.title = Some("private title".into());
        snapshot.warnings = BTreeSet::from([
            HistoryFidelityWarning::AlternateNodes,
            HistoryFidelityWarning::AttachmentNotImported,
            HistoryFidelityWarning::UnsupportedContent,
            HistoryFidelityWarning::UnsupportedRole,
            HistoryFidelityWarning::AttachmentMetadata,
            HistoryFidelityWarning::InvalidTimestamp,
        ]);
        let warnings = snapshot.import_warnings();
        assert_eq!(warnings.len(), snapshot.warnings.len());
        let codes: BTreeSet<_> = warnings.iter().map(|warning| &warning.code).collect();
        assert_eq!(codes.len(), warnings.len());
        for warning in &warnings {
            assert!(!warning.message.is_empty());
            assert!(!warning.message.contains("private title"));
            assert_eq!(warning.count, None);
        }
        let serialized = serde_json::to_vec(&warnings).unwrap();
        let decoded: Vec<bcode_session_import::ImportWarning> =
            serde_json::from_slice(&serialized).unwrap();
        assert_eq!(decoded, warnings);
        snapshot.warnings.clear();
        assert!(snapshot.import_warnings().is_empty());
    }

    #[test]
    fn import_events_preserve_identity_without_executable_authority() {
        use bcode_session_import::ImportableSessionEventKind;
        let mut snapshot = decode(&graph()).unwrap();
        for role in ["tool", "system", "developer", "unknown", "assistant"] {
            snapshot.messages[0].role = role.into();
            snapshot.messages[0].recipient = Some("python".into());
            snapshot.messages[0].created_at = Some(1.25);
            let events = snapshot.import_events();
            assert_eq!(
                events[0].external_event_id.as_deref(),
                Some(snapshot.messages[0].node_id.as_str())
            );
            assert_eq!(events[0].timestamp_ms, Some(1250));
            let ImportableSessionEventKind::AssistantMessage { text } = &events[0].kind else {
                panic!("historical activity must remain inert text");
            };
            assert!(text.starts_with("[Historical "));
            assert!(text.ends_with(&snapshot.messages[0].text));
        }
        for invalid in [f64::NAN, f64::INFINITY, -1.0, f64::MAX] {
            assert_eq!(timestamp_ms(invalid), None);
        }
    }

    #[test]
    fn revision_identity_tracks_normalized_content_not_wire_formatting() {
        let wire = graph();
        let snapshot = decode(&wire).unwrap();
        let original = snapshot.revision_id().unwrap();
        let pretty = serde_json::to_vec_pretty(&wire).unwrap();
        assert_eq!(
            original,
            decode_history(&pretty, "api-id", 65536)
                .unwrap()
                .revision_id()
                .unwrap()
        );
        for change in 0..4 {
            let mut changed = snapshot.clone();
            match change {
                0 => changed.title = Some("Renamed".into()),
                1 => changed.messages[0].text = "edited".into(),
                2 => changed.selected_node = "alternative".into(),
                _ => {
                    changed
                        .warnings
                        .insert(HistoryFidelityWarning::UnsupportedContent);
                }
            }
            assert_ne!(original, changed.revision_id().unwrap());
        }
        let mut ignored = wire;
        ignored["irrelevant_transport_metadata"] = json!("not persisted");
        assert_eq!(original, decode(&ignored).unwrap().revision_id().unwrap());
    }

    #[test]
    fn unrepresentable_timestamps_surface_fidelity_loss_before_import() {
        for timestamp in [
            serde_json::json!(1e30),
            serde_json::json!(-1),
            serde_json::json!("unknown"),
        ] {
            let mut source = graph();
            source["mapping"]["answer"]["message"]["create_time"] = timestamp;
            let snapshot = decode(&source).unwrap();
            assert!(
                snapshot
                    .warnings
                    .contains(&HistoryFidelityWarning::InvalidTimestamp)
            );
            assert!(snapshot.messages.last().unwrap().created_at.is_none());
            assert!(
                snapshot
                    .import_events()
                    .last()
                    .unwrap()
                    .timestamp_ms
                    .is_none()
            );
        }
        let mut source = graph();
        source["mapping"]["answer"]["message"]["create_time"] = json!(1.25);
        let snapshot = decode(&source).unwrap();
        assert!(
            !snapshot
                .warnings
                .contains(&HistoryFidelityWarning::InvalidTimestamp)
        );
        assert_eq!(
            snapshot.import_events().last().unwrap().timestamp_ms,
            Some(1250)
        );
    }

    #[test]
    fn duplicate_graph_keys_never_silently_replace_source_nodes() {
        // Raw JSON is intentional: constructing a Value would already discard duplicates.
        let bytes = br#"{"conversation_id":"api-id","current_node":"node","mapping":{
            "node":{"id":"node","parent":null,"message":null},
            "node":{"id":"node","parent":null,"message":null}
        }}"#;
        assert_eq!(
            decode_history(bytes, "api-id", 65536),
            Err(HistoryDecodeError::InvalidSchema)
        );
        assert_eq!(
            decode_history_branch(bytes, "api-id", 65536, Some("node")),
            Err(HistoryDecodeError::InvalidSchema)
        );
    }

    #[test]
    fn selected_in_progress_messages_never_form_complete_snapshots() {
        let mut source = graph();
        source["mapping"]["answer"]["message"]["status"] = json!("in_progress");
        assert_eq!(decode(&source), Err(HistoryDecodeError::Incomplete));
        let bytes = serde_json::to_vec(&source).unwrap();
        // An unrelated branch may still be imported without mixing partial output.
        let alternative =
            decode_history_branch(&bytes, "api-id", 65536, Some("alternative")).unwrap();
        assert_eq!(alternative.messages.last().unwrap().text, "other answer");
        source["mapping"]["answer"]["message"]["status"] = json!("finished_successfully");
        assert!(decode(&source).is_ok());
    }

    #[test]
    fn unknown_or_malformed_message_status_never_implies_completion() {
        for status in [
            json!("future_status"),
            json!(""),
            json!(null),
            json!(false),
            json!(42),
            json!({}),
        ] {
            let mut source = graph();
            source["mapping"]["answer"]["message"]["status"] = status;
            assert_eq!(decode(&source), Err(HistoryDecodeError::InvalidSchema));
        }
        // Older payloads without lifecycle metadata retain their existing interpretation.
        assert!(decode(&graph()).is_ok());
    }

    #[test]
    fn explicit_branch_selection_never_merges_or_falls_back() {
        let bytes = serde_json::to_vec(&graph()).unwrap();
        let alternative =
            decode_history_branch(&bytes, "api-id", 65536, Some("alternative")).unwrap();
        assert_eq!(alternative.selected_node, "alternative");
        assert_eq!(
            alternative
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["question", "other answer"]
        );
        assert_eq!(
            decode_history_branch(&bytes, "api-id", 65536, Some("missing")),
            Err(HistoryDecodeError::InvalidGraph)
        );
        let selected = decode_history(&bytes, "api-id", 65536).unwrap();
        assert_eq!(selected.selected_node, "answer");
        assert_eq!(selected.messages.last().unwrap().text, "selected answer");
    }

    #[test]
    fn historical_tool_text_and_provenance_are_preserved_as_data() {
        for content_type in ["code", "execution_output"] {
            let mut source = graph();
            source["mapping"]["answer"]["message"] = json!({
                "id": "message-id", "author": {"role": "tool", "name": "python"},
                "recipient": "all", "metadata": {"model_slug": "source-model"},
                "content": {"content_type": content_type, "text": "historical output"}
            });
            let snapshot = decode(&source).unwrap();
            let message = snapshot.messages.last().unwrap();
            assert_eq!(message.node_id, "answer");
            assert_eq!(message.message_id.as_deref(), Some("message-id"));
            assert_eq!(message.model_slug.as_deref(), Some("source-model"));
            assert_eq!(message.author_name.as_deref(), Some("python"));
            assert_eq!(message.recipient.as_deref(), Some("all"));
            assert_eq!(message.content_type.as_deref(), Some(content_type));
            assert_eq!(message.text, "historical output");
            assert!(
                !snapshot
                    .warnings
                    .contains(&HistoryFidelityWarning::UnsupportedContent)
            );
            source["mapping"]["answer"]["message"]["content"]["text"] = json!(42);
            assert_eq!(decode(&source), Err(HistoryDecodeError::InvalidSchema));
        }
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
        assert_eq!(
            result.messages[1].text,
            "caption\n[Historical non-text content not imported; no local backup]"
        );
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
