//! Version-one normalized model-host operations for admitted integration plugins.
//!
//! These requests never select credentials, provider settings, model profiles, or URLs.
//! The host binds those from admission configuration and binds caller identity from its bridge.
use serde::{Deserialize, Serialize};

/// Supported model-host interface.
pub const INTERFACE_ID: &str = "bcode.model-host/v1";
/// Start operation.
pub const OP_START: &str = "start_turn";
/// Poll operation.
pub const OP_POLL: &str = "poll_turn_events";
/// Cancel operation.
pub const OP_CANCEL: &str = "cancel_turn";
/// Cleanup operation.
pub const OP_FINISH: &str = "finish_turn";

/// Public text-only message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextRole {
    /// System instructions.
    System,
    /// User input.
    User,
    /// Previous assistant output.
    Assistant,
}

/// A text-only message, with no provider escape hatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextMessage {
    /// Normalized role.
    pub role: TextRole,
    /// UTF-8 message text.
    pub text: String,
}

/// Start one turn under a plugin-validated scope. Identical request IDs are idempotent
/// within the scope until its bounded retention expires; changed payloads conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    /// Internal lease identity, never the bearer token.
    pub scope_id: String,
    /// Absolute UNIX-seconds expiry; cannot exceed host admission limits.
    pub expires_at: u64,
    /// Caller-selected idempotency key.
    pub request_id: String,
    /// Normalized conversation. Validation admits text, tool calls, and text tool results only.
    #[serde(deserialize_with = "deserialize_public_messages")]
    pub messages: Vec<crate::ModelMessage>,
    /// Tool schemas supplied by the caller; no host tool execution is implied.
    #[serde(default, deserialize_with = "deserialize_public_tools")]
    pub tools: Vec<crate::ToolDefinition>,
    /// Normalized model tool-selection policy.
    #[serde(default, deserialize_with = "deserialize_public_tool_policy")]
    pub tool_call_policy: crate::ToolCallRequestPolicy,
    /// Optional normalized schema enforcement, negotiated against the fixed model.
    #[serde(default)]
    pub tool_schema_mode: Option<crate::ToolSchemaMode>,
    /// Optional output bound, capped by host policy.
    pub max_output_tokens: Option<u32>,
}

/// Decode normalized messages without silently ignoring continuation/private fields.
///
/// # Errors
/// Rejects unknown message/content fields, unsupported content and non-text tool-result content.
pub fn deserialize_public_messages<'de, D>(
    deserializer: D,
) -> Result<Vec<crate::ModelMessage>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = serde_json::Value::deserialize(deserializer)?;
    check_message_wire(&value).map_err(D::Error::custom)?;
    serde_json::from_value(value).map_err(D::Error::custom)
}

/// Decode tool definitions while rejecting extra provider-owned fields.
///
/// # Errors
/// Rejects unknown fields, invalid definitions, or more than 128 tools.
pub fn deserialize_public_tools<'de, D>(
    deserializer: D,
) -> Result<Vec<crate::ToolDefinition>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = serde_json::Value::deserialize(deserializer)?;
    let tools = value
        .as_array()
        .ok_or_else(|| D::Error::custom("invalid_tools"))?;
    if tools.len() > 128 {
        return Err(D::Error::custom("tool_bounds_exceeded"));
    }
    for tool in tools {
        fields(tool, &["name", "description", "input_schema"]).map_err(D::Error::custom)?;
    }
    serde_json::from_value(value).map_err(D::Error::custom)
}

/// Decode normalized tool policy without ignoring provider-private fields.
///
/// # Errors
/// Rejects unknown fields or unrecognized tool selection policy.
pub fn deserialize_public_tool_policy<'de, D>(
    deserializer: D,
) -> Result<crate::ToolCallRequestPolicy, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = serde_json::Value::deserialize(deserializer)?;
    fields(&value, &["parallel", "choice"]).map_err(D::Error::custom)?;
    if let Some(choice) = value.get("choice") {
        let allowed = if choice.get("mode").and_then(serde_json::Value::as_str) == Some("tool") {
            &["mode", "name"][..]
        } else {
            &["mode"][..]
        };
        fields(choice, allowed).map_err(D::Error::custom)?;
    }
    serde_json::from_value(value).map_err(D::Error::custom)
}

fn fields(value: &serde_json::Value, allowed: &[&str]) -> Result<(), &'static str> {
    let object = value.as_object().ok_or("invalid_public_content")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("unknown_public_content_field");
    }
    Ok(())
}

fn check_message_wire(value: &serde_json::Value) -> Result<(), &'static str> {
    let messages = value.as_array().ok_or("invalid_messages")?;
    if messages.len() > 128 {
        return Err("message_bounds_exceeded");
    }
    for message in messages {
        fields(message, &["role", "content"])?;
        let blocks = message
            .get("content")
            .and_then(serde_json::Value::as_array)
            .ok_or("invalid_content")?;
        if blocks.len() > 256 {
            return Err("content_bounds_exceeded");
        }
        for block in blocks {
            check_block_wire(block)?;
        }
    }
    Ok(())
}

fn check_block_wire(block: &serde_json::Value) -> Result<(), &'static str> {
    match block.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => fields(block, &["type", "text"]),
        Some("tool_call") => {
            fields(block, &["type", "call"])?;
            fields(
                block.get("call").ok_or("invalid_call")?,
                &["id", "name", "arguments"],
            )
        }
        Some("tool_result") => {
            fields(block, &["type", "result"])?;
            let result = block.get("result").ok_or("invalid_result")?;
            fields(result, &["call_id", "output", "is_error", "content"])?;
            if let Some(contents) = result.get("content") {
                let contents = contents.as_array().ok_or("invalid_result")?;
                if contents.len() > 256 {
                    return Err("content_bounds_exceeded");
                }
                for content in contents {
                    fields(content, &["type", "text"])?;
                    if content.get("type").and_then(serde_json::Value::as_str) != Some("text") {
                        return Err("unsupported_result_content");
                    }
                }
            }
            Ok(())
        }
        _ => Err("unsupported_public_content"),
    }
}

impl StartRequest {
    /// Validate bounded public content before reserving or dispatching a turn.
    ///
    /// # Errors
    /// Rejects private extensions, images, invalid tool relationships, oversized payloads,
    /// duplicate tool names, and schemas with external references or excessive complexity.
    pub fn validate_public(&self) -> Result<(), &'static str> {
        use crate::{ContentBlock, MessageRole, ToolChoice, ToolResultContent};
        use std::collections::{BTreeMap, BTreeSet};
        if self.messages.is_empty()
            || self.messages.len() > 128
            || self.tools.len() > 128
            || serde_json::to_vec(self)
                .map_err(|_| "invalid_request")?
                .len()
                > 196_608
        {
            return Err("request_bounds_exceeded");
        }
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            if !valid_name(&tool.name)
                || !names.insert(tool.name.as_str())
                || tool.description.len() > 8192
                || !tool.input_schema.is_object()
                || tool
                    .input_schema
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    != Some("object")
                || serde_json::to_vec(&tool.input_schema)
                    .map_err(|_| "invalid_schema")?
                    .len()
                    > 32_768
            {
                return Err("invalid_tool_definition");
            }
            validate_schema_tree(&tool.input_schema)?;
        }
        match &self.tool_call_policy.choice {
            ToolChoice::Tool { name } if !names.contains(name.as_str()) => {
                return Err("unknown_selected_tool");
            }
            ToolChoice::Required if self.tools.is_empty() => return Err("tools_required"),
            _ => {}
        }
        if self.tools.is_empty()
            && (self.tool_schema_mode.is_some() || self.tool_call_policy.parallel == Some(true))
        {
            return Err("tools_required");
        }
        let mut calls = BTreeMap::new();
        for message in &self.messages {
            if message.content.is_empty() || message.content.len() > 256 {
                return Err("invalid_message");
            }
            for block in &message.content {
                match block {
                    ContentBlock::Text { .. } if message.role != MessageRole::Tool => {}
                    ContentBlock::ToolCall { call } if message.role == MessageRole::Assistant => {
                        if !valid_id(&call.id)
                            || !valid_name(&call.name)
                            || !call.arguments.is_object()
                            || calls.insert(call.id.as_str(), false).is_some()
                        {
                            return Err("invalid_tool_call");
                        }
                    }
                    ContentBlock::ToolResult { result } if message.role == MessageRole::Tool => {
                        let completed = calls
                            .get_mut(result.call_id.as_str())
                            .ok_or("unknown_tool_result")?;
                        if *completed
                            || result
                                .content
                                .iter()
                                .any(|content| !matches!(content, ToolResultContent::Text { .. }))
                        {
                            return Err("invalid_tool_result");
                        }
                        *completed = true;
                    }
                    _ => return Err("unsupported_public_content"),
                }
            }
        }
        if calls.values().any(|completed| !completed) {
            return Err("missing_tool_result");
        }
        Ok(())
    }
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn validate_schema_tree(schema: &serde_json::Value) -> Result<(), &'static str> {
    let mut pending = vec![(schema, 0_u8)];
    let mut nodes = 0;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if nodes > 4096 || depth > 32 {
            return Err("schema_bounds_exceeded");
        }
        match value {
            serde_json::Value::Object(object) => {
                // Never let caller schemas initiate remote resolution or alter reference bases.
                if object.contains_key("$id")
                    || object.contains_key("$dynamicRef")
                    || object.get("$schema").is_some_and(|value| {
                        !matches!(
                            value.as_str(),
                            Some(
                                "http://json-schema.org/draft-07/schema#"
                                    | "https://json-schema.org/draft/2020-12/schema"
                                    | "https://json-schema.org/draft/2019-09/schema"
                            )
                        )
                    })
                    || object.contains_key("$recursiveRef")
                    || object.get("$ref").is_some_and(|value| {
                        value.as_str().is_none_or(|reference| {
                            !reference.starts_with("#/$defs/")
                                && !reference.starts_with("#/definitions/")
                        })
                    })
                {
                    return Err("external_schema_reference");
                }
                pending.extend(object.values().map(|child| (child, depth + 1)));
            }
            serde_json::Value::Array(array) => {
                pending.extend(array.iter().map(|child| (child, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Reserved opaque host turn identity. Reservation does not prove provider start succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartResponse {
    /// Host identity, never a provider turn ID.
    pub turn_id: String,
}

/// Scope-bound lifecycle request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnRequest {
    /// Internal lease identity.
    pub scope_id: String,
    /// Reserved host identity.
    pub turn_id: String,
}

/// Read retained events strictly after the supplied sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollRequest {
    /// Internal lease identity.
    pub scope_id: String,
    /// Reserved host identity.
    pub turn_id: String,
    /// Last consumed sequence; zero begins the stream. An evicted cursor fails explicitly.
    pub after_sequence: u64,
}

/// Retained public event. Hosts exclude provider-private extensions and reasoning state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequencedEvent {
    /// Monotonically increasing sequence, starting at one.
    pub sequence: u64,
    /// Safe normalized event selected by the host.
    pub event: crate::ProviderTurnEvent,
}

/// A bounded event page. Retention is process-local, not durable resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollResponse {
    /// Events in increasing sequence order.
    pub events: Vec<SequencedEvent>,
    /// Last sequence included, or the supplied cursor when the page is empty.
    pub next_sequence: u64,
    /// Whether the provider reached a terminal state (possibly with more retained pages).
    pub terminal: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coding_tool_round_trip_and_private_rejection() {
        let base = serde_json::json!({
            "scope_id":"s", "expires_at":200, "request_id":"r", "max_output_tokens":1024,
            "messages":[
                {"role":"user","content":[{"type":"text","text":"read file"}]},
                {"role":"assistant","content":[{"type":"tool_call","call":{"id":"call1","name":"read_file","arguments":{"path":"src/lib.rs"}}}]},
                {"role":"tool","content":[{"type":"tool_result","result":{"call_id":"call1","output":"contents","is_error":false,"content":[]}}]}
            ],
            "tools":[{"name":"read_file","description":"Read a file","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}],
            "tool_call_policy":{"choice":{"mode":"auto"},"parallel":false}
        });
        let request: StartRequest = serde_json::from_value(base.clone()).unwrap();
        assert!(request.validate_public().is_ok());
        let mut private = base.clone();
        private["messages"][1]["content"][0]["call"]["provider_private_data"] =
            serde_json::json!({"opaque":"secret"});
        assert!(serde_json::from_value::<StartRequest>(private).is_err());
        let mut image = base;
        image["messages"][0]["content"][0] = serde_json::json!({"type":"image","image":{}});
        assert!(serde_json::from_value::<StartRequest>(image).is_err());
        let mut remote = request.clone();
        remote.tools[0].input_schema =
            serde_json::json!({"type":"object","$ref":"https://untrusted/schema"});
        assert!(remote.validate_public().is_err());
        let mut missing = request;
        missing.messages.pop();
        assert!(missing.validate_public().is_err());
    }

    #[test]
    fn rejects_provider_context_and_unknown_roles() {
        let input = r#"{"scope_id":"s","expires_at":1,"request_id":"r","messages":[],"max_output_tokens":null,"provider_context":{"auth":{}}}"#;
        assert!(serde_json::from_str::<StartRequest>(input).is_err());
        assert!(serde_json::from_str::<TextMessage>(r#"{"role":"tool","text":"x"}"#).is_err());
    }
}
