//! SDK MCP parent-assistance protocol. Authorization comes only from host admission.
//! No native message, parent relationship, tool name, or prompt can grant bridge scope.
use bcode_claude_code::{CallbackRequest, CallbackResponse};
use bcode_harness_models::{
    HarnessInteraction, HarnessInteractionKind, HarnessSessionTarget, MAX_HARNESS_TEXT_BYTES,
};
use serde_json::{Value, json};

pub const SERVER: &str = "bcode-parent";
pub const TOOL: &str = "request_parent_assistance";

/// A pending call is held by the native transport while normalized interactions are polled.
/// Returning this value does not respond to or execute the tool.
pub enum BridgeCall {
    Reply(CallbackResponse),
    Pending(HarnessInteraction),
}

/// Interpret only this SDK server. `admitted_scope` is process-local host admission
/// state, never a value copied from native arguments. None exposes no tools.
pub fn handle(
    request_id: &str,
    request: &CallbackRequest,
    binding: &HarnessSessionTarget,
    admitted_scope: Option<&str>,
) -> Option<BridgeCall> {
    let CallbackRequest::McpMessage {
        server_name,
        message,
    } = request
    else {
        return None;
    };
    if server_name != SERVER {
        return None;
    }
    let valid_scope = admitted_scope
        .filter(|scope| !scope.trim().is_empty() && scope.len() <= MAX_HARNESS_TEXT_BYTES);
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let valid_id = matches!(&id, Value::Number(value) if value.is_i64() || value.is_u64())
        || matches!(&id, Value::String(value) if !value.is_empty() && value.len() <= 256);
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !valid_id {
        return Some(error(id, -32600, "Invalid MCP request envelope"));
    }
    match message.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            let protocol = message
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str);
            if !matches!(protocol, Some("2024-11-05" | "2025-03-26")) {
                return Some(error(id, -32602, "Unsupported MCP protocol version"));
            }
            Some(result(
                id,
                json!({"protocolVersion":protocol,"capabilities":{"tools":{}},"serverInfo":{"name":SERVER,"version":"1"}}),
            ))
        }
        Some("ping") => Some(result(id, json!({}))),
        Some("tools/list") => {
            let tools = if valid_scope.is_some() {
                vec![json!({
                    "name":TOOL,
                    "description":"Ask the host-authorized parent for bounded textual assistance. This does not execute parent tools or expand permissions.",
                    "inputSchema":{"type":"object","properties":{"question":{"type":"string","minLength":1,"maxLength":MAX_HARNESS_TEXT_BYTES}},"required":["question"],"additionalProperties":false}
                })]
            } else {
                Vec::new()
            };
            Some(result(id, json!({"tools":tools})))
        }
        Some("tools/call") => {
            let Some(scope) = valid_scope else {
                return Some(error(
                    id,
                    -32601,
                    "Parent assistance has no current host admission",
                ));
            };
            let Some(params) = message.get("params").and_then(Value::as_object) else {
                return Some(error(id, -32602, "Invalid assistance parameters"));
            };
            if params.get("name").and_then(Value::as_str) != Some(TOOL) {
                return Some(error(id, -32601, "Unknown parent assistance tool"));
            }
            let Some(arguments) = params.get("arguments").and_then(Value::as_object) else {
                return Some(error(id, -32602, "Invalid assistance arguments"));
            };
            let Some(question) = arguments.get("question").and_then(Value::as_str) else {
                return Some(error(id, -32602, "Assistance question required"));
            };
            if arguments.len() != 1
                || question.trim().is_empty()
                || question.len() > MAX_HARNESS_TEXT_BYTES
            {
                return Some(error(id, -32602, "Invalid bounded assistance question"));
            }
            let interaction = HarnessInteraction {
                input_preview: None,
                tool_name: None,
                request_id: request_id.into(),
                session_id: binding.session_id().to_owned(),
                generation: binding.generation(),
                kind: HarnessInteractionKind::Assistance,
                prompt: question.into(),
                scope: scope.into(),
                choices: Vec::new(),
            };
            if interaction.validate().is_err() || binding.validate().is_err() {
                return Some(error(id, -32602, "Invalid assistance binding"));
            }
            Some(BridgeCall::Pending(interaction))
        }
        _ => Some(error(id, -32601, "Unsupported parent bridge method")),
    }
}
fn result(id: Value, value: Value) -> BridgeCall {
    let mut response = serde_json::Map::new();
    response.insert("jsonrpc".into(), Value::String("2.0".into()));
    response.insert("id".into(), id);
    response.insert("result".into(), value);
    BridgeCall::Reply(CallbackResponse::Mcp {
        response: Value::Object(response),
    })
}
fn error(id: Value, code: i32, message: &str) -> BridgeCall {
    let mut response = serde_json::Map::new();
    response.insert("jsonrpc".into(), Value::String("2.0".into()));
    response.insert("id".into(), id);
    response.insert("error".into(), json!({"code":code,"message":message}));
    BridgeCall::Reply(CallbackResponse::Mcp {
        response: Value::Object(response),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::HarnessSessionBinding;
    fn binding() -> HarnessSessionTarget {
        HarnessSessionTarget::Confirmed(HarnessSessionBinding {
            schema_version: 1,
            session_id: "child".into(),
            instance_id: "instance".into(),
            native_session_id: "native".into(),
            native_version: "tested".into(),
            generation: 9,
        })
    }
    fn request(method: &str, params: &Value) -> CallbackRequest {
        CallbackRequest::McpMessage {
            server_name: SERVER.into(),
            message: json!({"jsonrpc":"2.0","id":"native-rpc","method":method,"params":params}),
        }
    }
    #[test]
    fn only_admitted_scope_exposes_the_real_tool() {
        let call = request("tools/list", &json!({}));
        let Some(BridgeCall::Reply(CallbackResponse::Mcp { response })) =
            handle("callback", &call, &binding(), None)
        else {
            panic!("expected discovery")
        };
        assert_eq!(response["result"]["tools"], json!([]));
        let Some(BridgeCall::Reply(CallbackResponse::Mcp { response })) =
            handle("callback", &call, &binding(), Some("host scope"))
        else {
            panic!("expected discovery")
        };
        assert_eq!(response["result"]["tools"][0]["name"], TOOL);
    }
    #[test]
    fn admitted_call_becomes_exact_generation_pending_assistance() {
        let call = request(
            "tools/call",
            &json!({"name":TOOL,"arguments":{"question":"Which API?"}}),
        );
        let Some(BridgeCall::Pending(interaction)) =
            handle("callback", &call, &binding(), Some("host grant; text only"))
        else {
            panic!("expected pending assistance")
        };
        assert_eq!(interaction.kind, HarnessInteractionKind::Assistance);
        assert_eq!(interaction.generation, 9);
        assert_eq!(interaction.request_id, "callback");
        assert_eq!(interaction.scope, "host grant; text only");
        assert_eq!(interaction.prompt, "Which API?");
        assert!(matches!(
            handle("callback", &call, &binding(), None),
            Some(BridgeCall::Reply(_))
        ));
    }
    #[test]
    fn arbitrary_tools_and_native_scope_injection_fail_closed() {
        for params in [
            json!({"name":"Bash","arguments":{"question":"help"}}),
            json!({"name":TOOL,"arguments":{"question":"help","scope":"admin"}}),
        ] {
            assert!(matches!(
                handle(
                    "callback",
                    &request("tools/call", &params),
                    &binding(),
                    Some("host scope")
                ),
                Some(BridgeCall::Reply(_))
            ));
        }
    }
}
