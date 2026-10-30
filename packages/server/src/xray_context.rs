//! Transient context diagnostics. These observations never participate in context accounting.
//!
//! Sensitive encodings are lazy, bounded, and restricted to normalized request fields. Provider
//! context, request metadata, opaque extensions and image source paths are deliberately excluded.
use std::io::{self, Write};

use bcode_model::{ContentBlock, ModelMessage, ModelTurnRequest};
use bcode_xray::CaptureContext;
use serde::ser::{Serialize, SerializeSeq, SerializeStruct, Serializer};
use serde_json::json;

const INLINE_CAPTURE_BYTES: usize = 32 * 1024;
// Independent producer allocation ceiling. The engine may configure a smaller retention limit;
// its ready capabilities advertise that bound and capture_payload remains authoritative.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

/// Return the active capture context only when it belongs to the requested session.
pub fn current(session_id: bcode_session_models::SessionId) -> Option<CaptureContext> {
    CaptureContext::current()
        .filter(|context| context.correlation().session_id == session_id.to_string())
        .filter(CaptureContext::capture_active)
}

/// Capture the assembled normalized host request, not a provider wire request or token estimate.
/// The caller must invoke this after final request mutations and only for actual generation.
pub fn assembled_request(request: &ModelTurnRequest, boundary: &'static str) {
    let Some(context) = current(request.session_id) else {
        return;
    };
    let mut correlation = context.correlation().clone();
    correlation.request_id = Some(request.turn_id.clone());
    let context = context.with_correlation(correlation);
    context.emit(
        "context",
        boundary,
        json!({
            "messages": request.messages.len(),
            "tools": request.tools.len(),
            "has_system_prompt": request.system_prompt.is_some(),
            "has_structured_output": request.structured_output.is_some(),
            "lineage_available": request.context_lineage.is_some(),
            "representation": "normalized_host_request_not_provider_wire",
            "capture_completeness": "normalized_view_with_explicit_exclusions",
            "excluded_fields": ["provider_context", "metadata", "provider_extensions", "image_source_paths", "unresolved_image_reference_bytes", "prompt_cache", "conversation_reuse"],
            "inline_limit_bytes": INLINE_CAPTURE_BYTES,
            "producer_limit_bytes": MAX_CAPTURE_BYTES,
            "engine_limits": "see_xray_ready_capabilities",
        }),
        false,
    );
    sensitive(
        &context,
        "request_content",
        &RequestView {
            model_id: &request.model_id,
            context_lineage: &request.context_lineage,
            tool_call_policy: &request.tool_call_policy,
            tool_schema_mode: &request.tool_schema_mode,
            context_management: &request.context_management,
            system_prompt: &request.system_prompt,
            messages: Messages(&request.messages),
            tools: &request.tools,
            parameters: &request.parameters,
            structured_output: &request.structured_output,
            omissions: "provider_context,metadata,provider_extensions,image_source_paths,unresolved_image_reference_bytes,prompt_cache,conversation_reuse",
        },
    );
}

/// Observe final message assembly even when content-free lineage cannot be established.
pub fn final_messages(messages: &[ModelMessage]) {
    let Some(context) = CaptureContext::current().filter(CaptureContext::capture_active) else {
        return;
    };
    context.emit("context", "messages_assembled", json!({
        "messages": messages.len(),
        "capture_completeness": "normalized_view_with_explicit_exclusions",
        "excluded_fields": ["provider_extensions", "image_source_paths", "unresolved_image_reference_bytes"],
        "inline_limit_bytes": INLINE_CAPTURE_BYTES,
        "producer_limit_bytes": MAX_CAPTURE_BYTES,
        "engine_limits": "see_xray_ready_capabilities",
    }), false);
    sensitive(&context, "assembled_messages", &Messages(messages));
}

/// Encoding failures and size limits are explicit omissions, never partial JSON or raw errors.
/// Small normalized views are inline; larger views use observation-scoped JSON payload references.
/// `complete_normalized_view` describes byte completeness of this explicit projection only, not
/// provider-wire fidelity: excluded fields and unavailable image references remain declared.
/// The one MiB producer ceiling bounds allocation independently of configurable engine limits.
pub fn sensitive<T: Serialize + ?Sized>(context: &CaptureContext, name: &'static str, value: &T) {
    if !context.deep_capture_active() {
        return;
    }
    let bytes = match bounded_bytes(value) {
        Ok(bytes) => bytes,
        Err(reason) => {
            omitted(context, name, reason, None);
            return;
        }
    };
    let byte_len = bytes.len();
    if byte_len <= INLINE_CAPTURE_BYTES {
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) => context.emit(
                "context",
                name,
                json!({
                    "storage": "inline", "byte_len": byte_len,
                    "completeness": "complete_normalized_view", "content": value,
                    "inline_limit_bytes": INLINE_CAPTURE_BYTES,
                    "producer_limit_bytes": MAX_CAPTURE_BYTES,
                }),
                true,
            ),
            Err(_) => omitted(context, name, "encoding_failed", Some(byte_len)),
        }
        return;
    }
    match context.capture_payload("application/json", &bytes) {
        Ok(Some(payload)) => context.emit(
            "context",
            name,
            json!({
                "storage": "payload", "payload": payload, "byte_len": byte_len,
                "completeness": "complete_normalized_view",
                "inline_limit_bytes": INLINE_CAPTURE_BYTES,
                "producer_limit_bytes": MAX_CAPTURE_BYTES,
                "retention": "transient_observation_scoped_subject_to_eviction",
            }),
            true,
        ),
        Ok(None) => omitted(context, name, "capture_no_longer_active", Some(byte_len)),
        Err(error) => {
            let reason = match error {
                bcode_xray::XrayError::PayloadTooLarge => "engine_payload_limit",
                bcode_xray::XrayError::PayloadCaptureUnsupported => "payload_capture_unsupported",
                bcode_xray::XrayError::Busy => "engine_busy",
                _ => "payload_unavailable",
            };
            omitted(context, name, reason, Some(byte_len));
        }
    }
}

fn omitted(
    context: &CaptureContext,
    name: &'static str,
    reason: &'static str,
    byte_len: Option<usize>,
) {
    context.emit("context", "capture_omitted", json!({
        "capture": name, "reason": reason, "byte_len": byte_len,
        "completeness": "unavailable", "storage": "none",
        "producer_limit_bytes": MAX_CAPTURE_BYTES,
        "encoded_size_at_least": if reason == "byte_limit" { Some(MAX_CAPTURE_BYTES + 1) } else { None },
        "engine_limits": "see_xray_ready_capabilities",
    }), false);
}

fn bounded_bytes<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, &'static str> {
    let mut output = BoundedWriter {
        bytes: Vec::new(),
        exceeded: false,
    };
    if serde_json::to_writer(&mut output, value).is_err() {
        return Err(if output.exceeded {
            "byte_limit"
        } else {
            "encoding_failed"
        });
    }
    Ok(output.bytes)
}

#[cfg(test)]
fn bounded_value<T: Serialize + ?Sized>(value: &T) -> Result<serde_json::Value, &'static str> {
    serde_json::from_slice(&bounded_bytes(value)?).map_err(|_| "encoding_failed")
}

struct BoundedWriter {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_CAPTURE_BYTES.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("capture byte limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Do not serialize the whole ModelTurnRequest: it carries provider context and metadata.
#[derive(serde::Serialize)]
struct RequestView<'a> {
    model_id: &'a str,
    context_lineage: &'a Option<bcode_model::lineage::RequestContextLineage>,
    tool_call_policy: &'a bcode_model::ToolCallRequestPolicy,
    tool_schema_mode: &'a Option<bcode_model::ToolSchemaMode>,
    context_management: &'a bcode_model::ContextManagementRequest,
    system_prompt: &'a Option<String>,
    messages: Messages<'a>,
    tools: &'a [bcode_model::ToolDefinition],
    parameters: &'a bcode_model::ModelParameters,
    structured_output: &'a Option<bcode_model::StructuredOutputRequest>,
    omissions: &'static str,
}

struct Messages<'a>(&'a [ModelMessage]);

impl Serialize for Messages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for message in self.0 {
            sequence.serialize_element(&Message(message))?;
        }
        sequence.end()
    }
}

struct Message<'a>(&'a ModelMessage);

impl Serialize for Message<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut message = serializer.serialize_struct("Message", 2)?;
        message.serialize_field("role", &self.0.role)?;
        message.serialize_field("content", &Blocks(&self.0.content))?;
        message.end()
    }
}

struct Blocks<'a>(&'a [ContentBlock]);

impl Serialize for Blocks<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for block in self.0 {
            match block {
                ContentBlock::ProviderExtension { .. } => {
                    sequence.serialize_element(
                        &json!({"type": "omitted", "reason": "provider_extension"}),
                    )?;
                }
                ContentBlock::Image { image } => {
                    sequence.serialize_element(&ImageView::new(image))?;
                }
                ContentBlock::ToolResult { result } => {
                    #[derive(serde::Serialize)]
                    struct ToolResultView<'a> {
                        r#type: &'static str,
                        result: ToolResultBody<'a>,
                    }
                    #[derive(serde::Serialize)]
                    struct ToolResultBody<'a> {
                        call_id: &'a str,
                        output: &'a str,
                        is_error: bool,
                        content: ToolContent<'a>,
                    }
                    sequence.serialize_element(&ToolResultView {
                        r#type: "tool_result",
                        result: ToolResultBody {
                            call_id: &result.call_id,
                            output: &result.output,
                            is_error: result.is_error,
                            content: ToolContent(&result.content),
                        },
                    })?;
                }
                _ => sequence.serialize_element(block)?,
            }
        }
        sequence.end()
    }
}

/// Borrowed normalized image data; source paths and arbitrary metadata never enter capture.
#[derive(serde::Serialize)]
struct ImageView<'a> {
    r#type: &'static str,
    mime_type: &'a str,
    data_base64: &'a str,
    width: Option<u32>,
    height: Option<u32>,
    byte_len: Option<u64>,
    source_path_omitted: bool,
}

impl<'a> ImageView<'a> {
    fn new(image: &'a bcode_model::ImageContent) -> Self {
        Self {
            r#type: "image",
            mime_type: &image.mime_type,
            data_base64: &image.data_base64,
            width: image.metadata.width,
            height: image.metadata.height,
            byte_len: image.metadata.byte_len,
            source_path_omitted: image.metadata.source_path.is_some(),
        }
    }
}

struct ToolContent<'a>(&'a [bcode_model::ToolResultContent]);

impl Serialize for ToolContent<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for content in self.0 {
            match content {
                bcode_model::ToolResultContent::Text { .. } => {
                    sequence.serialize_element(content)?;
                }
                bcode_model::ToolResultContent::Image { image } => {
                    sequence.serialize_element(&ImageView::new(image))?;
                }
                bcode_model::ToolResultContent::ImageRef { image } => {
                    // Do not resolve paths, read files, or claim that a reference is image bytes.
                    #[derive(serde::Serialize)]
                    struct Reference<'a> {
                        r#type: &'static str,
                        mime_type: &'a str,
                        completeness: &'static str,
                        omissions: &'static [&'static str],
                    }
                    sequence.serialize_element(&Reference {
                        r#type: "image_ref",
                        mime_type: &image.mime_type,
                        completeness: "unavailable_image_bytes",
                        omissions: &["path", "artifact_id", "reference_key", "metadata"],
                    })?;
                }
            }
        }
        sequence.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_content_is_omitted_instead_of_truncated() {
        assert_eq!(
            bounded_value(&"x".repeat(MAX_CAPTURE_BYTES)),
            Err("byte_limit")
        );
        assert_eq!(
            bounded_value(&json!({"text": "small"})).unwrap(),
            json!({"text": "small"})
        );
    }

    #[test]
    fn sensitive_message_view_excludes_opaque_provider_data() {
        let messages = vec![ModelMessage {
            role: bcode_model::MessageRole::User,
            content: vec![
                ContentBlock::Text {
                    text: "visible context".into(),
                },
                ContentBlock::ProviderExtension {
                    value: json!({"private": "not exported"}),
                },
            ],
        }];
        let value = bounded_value(&Messages(&messages)).unwrap();
        assert_eq!(value[0]["content"][0]["text"], "visible context");
        assert_eq!(
            value[0]["content"][1],
            json!({"type": "omitted", "reason": "provider_extension"})
        );
    }

    #[tokio::test]
    async fn scalar_observation_never_serializes_sensitive_values() {
        struct MustNotSerialize;
        impl Serialize for MustNotSerialize {
            fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                panic!("sensitive serialization without deep capture");
            }
        }
        let hub = bcode_xray::XrayHub::default();
        let context = hub.context(bcode_xray::XrayCorrelation {
            session_id: "session".into(),
            ..Default::default()
        });
        sensitive(&context, "test", &MustNotSerialize);
        let mut observer = hub
            .observe(bcode_xray::XrayObserveRequest {
                schema_version: 1,
                session_id: "session".into(),
                deep_capture: false,
            })
            .unwrap();
        observer.recv().await.unwrap();
        sensitive(&context, "test", &MustNotSerialize);
        observer.finalize().unwrap();
        assert_eq!(observer.recv().await.unwrap().event.domain, "xray");
    }

    #[tokio::test]
    async fn deep_capture_reports_oversize_without_partial_content() {
        let hub = bcode_xray::XrayHub::default();
        let context = hub.context(bcode_xray::XrayCorrelation {
            session_id: "session".into(),
            ..Default::default()
        });
        let mut observer = hub
            .observe(bcode_xray::XrayObserveRequest {
                schema_version: 1,
                session_id: "session".into(),
                deep_capture: true,
            })
            .unwrap();
        observer.recv().await.unwrap();
        sensitive(&context, "test", &"x".repeat(MAX_CAPTURE_BYTES));
        let event = observer.recv().await.unwrap().event;
        assert_eq!(event.name, "capture_omitted");
        assert_eq!(event.data["reason"], "byte_limit");
        assert!(!event.sensitive);
        drop(observer);
        assert!(!context.deep_capture_active());
    }

    #[tokio::test]
    async fn large_assembled_request_is_retained_as_a_readable_payload() {
        let hub = bcode_xray::XrayHub::default();
        let session = "00000000-0000-0000-0000-000000000001";
        let context = hub.context(bcode_xray::XrayCorrelation {
            session_id: session.into(),
            ..Default::default()
        });
        let mut observer = hub
            .observe(bcode_xray::XrayObserveRequest {
                schema_version: 1,
                session_id: session.into(),
                deep_capture: true,
            })
            .unwrap();
        observer.recv().await.unwrap();
        let prompt = "x".repeat(INLINE_CAPTURE_BYTES * 2);
        let request: ModelTurnRequest = serde_json::from_value(json!({
            "session_id": session, "turn_id": "request-1", "model_id": "fixture",
            "system_prompt": prompt, "messages": [], "tools": [],
            "metadata": {"secret": "excluded"},
        }))
        .unwrap();
        context
            .scope(async {
                assembled_request(&request, "request_assembled");
            })
            .await;
        let scalar = observer.recv().await.unwrap();
        assert_eq!(scalar.event.name, "request_assembled");
        let captured = observer.recv().await.unwrap().event;
        assert_eq!(captured.name, "request_content");
        assert_eq!(captured.data["storage"], "payload");
        assert_eq!(
            captured.correlation.request_id.as_deref(),
            Some("request-1")
        );
        let reference: bcode_xray::XrayPayloadRef =
            serde_json::from_value(captured.data["payload"].clone()).unwrap();
        let mut bytes = Vec::new();
        while u64::try_from(bytes.len()).unwrap() < reference.byte_len {
            let range = hub
                .read_payload(&bcode_xray::XrayReadPayloadRequest {
                    schema_version: 1,
                    observation_id: observer.id().into(),
                    session_id: session.into(),
                    payload_id: reference.payload_id.clone(),
                    offset: u64::try_from(bytes.len()).unwrap(),
                    length: 16 * 1024,
                })
                .unwrap();
            assert!(!range.bytes.is_empty());
            bytes.extend_from_slice(&range.bytes);
        }
        let decoded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded["system_prompt"], prompt);
        assert!(decoded.get("metadata").is_none());
        assert!(decoded.get("provider_context").is_none());
        assert_eq!(captured.data["completeness"], "complete_normalized_view");
    }

    #[tokio::test]
    async fn large_message_view_uses_payload_reference_instead_of_omission() {
        let hub = bcode_xray::XrayHub::default();
        let context = hub.context(bcode_xray::XrayCorrelation {
            session_id: "session".into(),
            ..Default::default()
        });
        let mut observer = hub
            .observe(bcode_xray::XrayObserveRequest {
                schema_version: 1,
                session_id: "session".into(),
                deep_capture: true,
            })
            .unwrap();
        observer.recv().await.unwrap();
        let messages = vec![ModelMessage {
            role: bcode_model::MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "x".repeat(INLINE_CAPTURE_BYTES * 2),
            }],
        }];
        context
            .scope(async {
                final_messages(&messages);
            })
            .await;
        assert_eq!(
            observer.recv().await.unwrap().event.name,
            "messages_assembled"
        );
        let captured = observer.recv().await.unwrap().event;
        assert_eq!(captured.name, "assembled_messages");
        assert_eq!(captured.data["storage"], "payload");
        assert!(captured.data["payload"]["byte_len"].as_u64().unwrap() > 32 * 1024);
        assert_eq!(captured.data["completeness"], "complete_normalized_view");
    }

    #[tokio::test]
    async fn configured_engine_payload_limit_is_explicitly_unavailable() {
        let hub = bcode_xray::XrayHub::new(bcode_xray::XrayConfig {
            max_payload_bytes: INLINE_CAPTURE_BYTES,
            ..Default::default()
        });
        let context = hub.context(bcode_xray::XrayCorrelation {
            session_id: "session".into(),
            ..Default::default()
        });
        let mut observer = hub
            .observe(bcode_xray::XrayObserveRequest {
                schema_version: 1,
                session_id: "session".into(),
                deep_capture: true,
            })
            .unwrap();
        observer.recv().await.unwrap();
        sensitive(&context, "large", &"x".repeat(INLINE_CAPTURE_BYTES * 2));
        let event = observer.recv().await.unwrap().event;
        assert_eq!(event.name, "capture_omitted");
        assert_eq!(event.data["reason"], "engine_payload_limit");
        assert_eq!(event.data["completeness"], "unavailable");
    }

    #[test]
    fn structured_tool_content_preserves_text_and_image_bytes_without_paths() {
        let image = bcode_model::ImageContent {
            mime_type: "image/png".into(),
            data_base64: "aGVsbG8=".into(),
            metadata: bcode_model::ImageMetadata {
                source_path: Some("/private/image.png".into()),
                width: Some(12),
                ..Default::default()
            },
        };
        let content = vec![
            bcode_model::ToolResultContent::Text {
                text: "structured text".into(),
            },
            bcode_model::ToolResultContent::Image { image },
            bcode_model::ToolResultContent::ImageRef {
                image: bcode_model::ImageRefContent {
                    path: "/private/reference.png".into(),
                    mime_type: "image/png".into(),
                    artifact_id: Some("private-artifact".into()),
                    reference_key: Some("private-key".into()),
                    metadata: bcode_model::ImageMetadata::default(),
                },
            },
        ];
        let messages = vec![ModelMessage {
            role: bcode_model::MessageRole::Tool,
            content: vec![ContentBlock::ToolResult {
                result: bcode_model::ToolResult {
                    call_id: "tool-1".into(),
                    output: "plain output".into(),
                    is_error: false,
                    content,
                },
            }],
        }];
        let value = bounded_value(&Messages(&messages)).unwrap();
        let result = &value[0]["content"][0]["result"];
        assert_eq!(result["output"], "plain output");
        assert_eq!(result["content"][0]["text"], "structured text");
        assert_eq!(result["content"][1]["data_base64"], "aGVsbG8=");
        assert_eq!(result["content"][1]["width"], 12);
        assert!(result["content"][1].get("source_path").is_none());
        assert!(result["content"][2].get("path").is_none());
        assert!(result["content"][2].get("artifact_id").is_none());
        assert!(result["content"][2].get("reference_key").is_none());
        assert_eq!(
            result["content"][2]["completeness"],
            "unavailable_image_bytes"
        );
    }

    #[test]
    fn writer_never_retains_more_than_its_byte_budget() {
        let mut writer = BoundedWriter {
            bytes: Vec::new(),
            exceeded: false,
        };
        writer.write_all(&vec![b'a'; MAX_CAPTURE_BYTES]).unwrap();
        assert!(writer.write_all(b"b").is_err());
        assert_eq!(writer.bytes.len(), MAX_CAPTURE_BYTES);
        assert!(writer.exceeded);
    }
}
