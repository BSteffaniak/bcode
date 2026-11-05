//! Distinct SDK MCP queue for host-authorized ordinary Bcode tools. Never calls back into
//! the daemon synchronously: native callbacks enqueue, host drains, host completes later.
use bcode_claude_code::{CallbackRequest, CallbackResponse};
use bcode_harness_models::{
    MANAGED_TOOL_BRIDGE_VERSION, MAX_HARNESS_ID_BYTES, MAX_MANAGED_TOOL_QUEUE,
    ManagedToolOperation, ManagedToolOutcome, ManagedToolQueueScope, ManagedToolRequest,
    ManagedToolResponse,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

/// SDK server identity; its name alone grants no authority.
pub const SERVER: &str = "bcode-tools";

/// A recognized callback is either queued or answered immediately.
pub enum CallbackAction {
    Queued,
    Reply(CallbackResponse),
}

struct Pending {
    callback_id: String,
    rpc_id: Value,
    operation: Operation,
}
enum Operation {
    Catalog,
    Prepare { invocation_id: String },
    Invoke,
}
#[derive(Default)]
pub struct Queue {
    scope: Option<ManagedToolQueueScope>,
    pending: BTreeMap<String, Pending>,
    queued: VecDeque<ManagedToolRequest>,
    next: u64,
}
impl Queue {
    pub fn bind(&mut self, scope: ManagedToolQueueScope) -> bool {
        if scope.validate().is_err() {
            return false;
        }
        if let Some(existing) = &self.scope {
            return existing == &scope;
        }
        self.scope = Some(scope);
        true
    }
    pub const fn scope(&self) -> Option<&ManagedToolQueueScope> {
        self.scope.as_ref()
    }
    pub fn drain(&mut self, scope: &ManagedToolQueueScope) -> Option<Vec<ManagedToolRequest>> {
        if self.scope.as_ref() != Some(scope) {
            return None;
        }
        Some(self.queued.drain(..).collect())
    }
    pub fn revoke(
        &mut self,
        scope: &ManagedToolQueueScope,
    ) -> Option<Vec<(String, CallbackResponse)>> {
        if self.scope.as_ref() != Some(scope) {
            return None;
        }
        self.scope = None;
        self.queued.clear();
        Some(
            std::mem::take(&mut self.pending)
                .into_values()
                .map(|pending| {
                    (
                        pending.callback_id,
                        error(&pending.rpc_id, "Host tool grant revoked"),
                    )
                })
                .collect(),
        )
    }
    /// Returns an immediate reply for unsupported input, or queues a callback without replying.
    pub fn callback(
        &mut self,
        callback_id: &str,
        request: &CallbackRequest,
    ) -> Option<CallbackAction> {
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
        let rpc_id = message.get("id").cloned().unwrap_or(Value::Null);
        let valid_id = matches!(&rpc_id, Value::Number(value) if value.is_i64() || value.is_u64())
            || matches!(&rpc_id, Value::String(value) if !value.is_empty() && value.len() <= 256);
        if let Some(result) = startup_result(message, self.scope.is_some(), valid_id) {
            let response_id = if rpc_id.is_null() { json!(0) } else { rpc_id };
            return Some(CallbackAction::Reply(reply(&response_id, &result)));
        }
        if !self.accepts_callback(callback_id, valid_id, message) {
            return Some(CallbackAction::Reply(error(
                &rpc_id,
                "Host tool queue unavailable",
            )));
        }
        let (operation, kind) = match message["method"].as_str() {
            Some("tools/list") => {
                let after = message["params"]["cursor"].as_str().map(str::to_owned);
                (
                    ManagedToolOperation::Catalog { after, limit: 64 },
                    Operation::Catalog,
                )
            }
            Some("tools/call") => {
                let Some(name) = message["params"]["name"].as_str() else {
                    return Some(CallbackAction::Reply(error(&rpc_id, "Tool name required")));
                };
                let Some(arguments) = message["params"]["arguments"].as_object() else {
                    return Some(CallbackAction::Reply(error(
                        &rpc_id,
                        "Tool arguments required",
                    )));
                };
                self.next = match self.next.checked_add(1) {
                    Some(next) => next,
                    None => return Some(CallbackAction::Reply(error(&rpc_id, "Queue exhausted"))),
                };
                let invocation_id = format!(
                    "mcp:{}:{}",
                    self.scope.as_ref().expect("bound").bridge_id,
                    self.next
                );
                (
                    ManagedToolOperation::Prepare {
                        invocation_id: invocation_id.clone(),
                        tool_name: name.to_owned(),
                        arguments: Value::Object(arguments.clone()),
                    },
                    Operation::Prepare { invocation_id },
                )
            }
            _ => {
                return Some(CallbackAction::Reply(error(
                    &rpc_id,
                    "Unsupported host tool method",
                )));
            }
        };
        self.next = match self.next.checked_add(1) {
            Some(next) => next,
            None => return Some(CallbackAction::Reply(error(&rpc_id, "Queue exhausted"))),
        };
        let request_id = format!("queue:{}", self.next);
        let request = ManagedToolRequest {
            version: MANAGED_TOOL_BRIDGE_VERSION,
            request_id: request_id.clone(),
            operation,
        };
        if request.validate().is_err() {
            return Some(CallbackAction::Reply(error(
                &rpc_id,
                "Invalid bounded tool request",
            )));
        }
        self.pending.insert(
            request_id,
            Pending {
                callback_id: callback_id.to_owned(),
                rpc_id,
                operation: kind,
            },
        );
        self.queued.push_back(request);
        Some(CallbackAction::Queued)
    }
    fn accepts_callback(&self, callback_id: &str, valid_id: bool, message: &Value) -> bool {
        self.scope.is_some()
            && valid_id
            && message["jsonrpc"] == "2.0"
            && !callback_id.is_empty()
            && callback_id.len() <= MAX_HARNESS_ID_BYTES
            && self.pending.len() < MAX_MANAGED_TOOL_QUEUE
            && !self
                .pending
                .values()
                .any(|pending| pending.callback_id == callback_id)
    }

    /// Consume an exact correlated host response. A prepared response queues invoke; it never
    /// executes locally or treats an ID/hash/native argument as permission.
    pub fn complete(
        &mut self,
        scope: &ManagedToolQueueScope,
        response: ManagedToolResponse,
    ) -> Result<Option<(String, CallbackResponse)>, ()> {
        if self.scope.as_ref() != Some(scope) || response.version != MANAGED_TOOL_BRIDGE_VERSION {
            return Err(());
        }
        let pending = self.pending.remove(&response.request_id).ok_or(())?;
        match (pending.operation, response.outcome) {
            (
                Operation::Prepare { invocation_id },
                ManagedToolOutcome::Prepared {
                    invocation_id: actual,
                    preparation_sha256,
                },
            ) if invocation_id == actual => {
                let request = ManagedToolRequest {
                    version: MANAGED_TOOL_BRIDGE_VERSION,
                    request_id: response.request_id.clone(),
                    operation: ManagedToolOperation::Invoke {
                        invocation_id,
                        preparation_sha256,
                    },
                };
                request.validate().map_err(|_| ())?;
                self.pending.insert(
                    response.request_id,
                    Pending {
                        callback_id: pending.callback_id,
                        rpc_id: pending.rpc_id,
                        operation: Operation::Invoke,
                    },
                );
                self.queued.push_back(request);
                Ok(None)
            }
            (Operation::Catalog, ManagedToolOutcome::Catalog { tools, next }) => {
                let tools = tools.into_iter().map(|tool| json!({"name":tool["name"],"description":tool["description"],"inputSchema":tool["input_schema"]})).collect::<Vec<_>>();
                let mut result = json!({"tools":tools});
                if let Some(next) = next {
                    result["nextCursor"] = json!(next);
                }
                Ok(Some((pending.callback_id, reply(&pending.rpc_id, &result))))
            }
            (Operation::Invoke, ManagedToolOutcome::Recorded { result }) => {
                let output = result["model_output"].as_str().ok_or(())?;
                let is_error = result["is_error"].as_bool().ok_or(())?;
                Ok(Some((
                    pending.callback_id,
                    reply(
                        &pending.rpc_id,
                        &json!({"content":[{"type":"text","text":output}],"isError":is_error}),
                    ),
                )))
            }
            (_, _) => Ok(Some((
                pending.callback_id,
                error(
                    &pending.rpc_id,
                    "Host tool denied, cancelled or uncertain; do not replay",
                ),
            ))),
        }
    }
}
fn reply(id: &Value, result: &Value) -> CallbackResponse {
    CallbackResponse::Mcp {
        response: json!({"jsonrpc":"2.0","id":id,"result":result}),
    }
}
fn startup_result(message: &Value, bound: bool, valid_id: bool) -> Option<Value> {
    if message["jsonrpc"] != "2.0" || (!valid_id && message.get("id").is_some()) {
        return None;
    }
    match message["method"].as_str()? {
        "initialize" => Some(json!({
            "protocolVersion":"2024-11-05",
            "capabilities":{"tools":{"listChanged":true}},
            "serverInfo":{"name":SERVER,"version":"1.0.0"},
        })),
        "ping" | "notifications/initialized" => Some(json!({})),
        "tools/list" if !bound => Some(json!({"tools":[]})),
        _ => None,
    }
}

fn error(id: &Value, message: &str) -> CallbackResponse {
    CallbackResponse::Mcp {
        response: json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":message}}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope() -> ManagedToolQueueScope {
        ManagedToolQueueScope {
            session_id: "session".into(),
            generation: 1,
            bridge_id: "host-issued".into(),
        }
    }
    fn callback() -> CallbackRequest {
        CallbackRequest::McpMessage {
            server_name: SERVER.into(),
            message: json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"filesystem.read","arguments":{"path":"a"}}}),
        }
    }
    #[test]
    fn inert_sdk_handshake_stays_healthy_without_grant_or_tool_execution() {
        let mut queue = Queue::default();
        for (id, method) in [
            (Some(json!(0)), "initialize"),
            (None, "notifications/initialized"),
            (Some(json!(1)), "tools/list"),
        ] {
            let mut message = json!({"jsonrpc":"2.0","method":method,
                "params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"claude-code","version":"2.1.308"}}});
            if let Some(id) = id {
                message["id"] = id;
            }
            let request = CallbackRequest::McpMessage {
                server_name: SERVER.into(),
                message,
            };
            let Some(CallbackAction::Reply(CallbackResponse::Mcp { response })) =
                queue.callback("startup", &request)
            else {
                panic!("inert handshake must answer synchronously");
            };
            assert!(response.get("error").is_none());
            if method == "initialize" {
                assert_eq!(
                    response["result"]["capabilities"],
                    json!({"tools":{"listChanged":true}})
                );
            }
            if method == "tools/list" {
                assert_eq!(response["result"]["tools"], json!([]));
            }
        }
        assert!(queue.scope().is_none());
        assert!(queue.pending.is_empty());
        assert!(queue.queued.is_empty());
        assert!(matches!(
            queue.callback("unbound-call", &callback()),
            Some(CallbackAction::Reply(_))
        ));
    }

    #[test]
    fn catalog_disposition_tracks_bind_and_exact_revocation_without_replay() {
        use crate::diagnostics::CallbackDisposition;
        let mut queue = Queue::default();
        let request = CallbackRequest::McpMessage {
            server_name: SERVER.into(),
            message: json!({"jsonrpc":"2.0","id":17,"method":"tools/list"}),
        };
        let action = queue.callback("unbound", &request);
        assert!(matches!(
            CallbackDisposition::classify(queue.scope().is_some(), action.as_ref()),
            CallbackDisposition::NoScope
        ));
        assert!(queue.bind(scope()));
        let action = queue.callback("bound", &request);
        assert!(matches!(
            CallbackDisposition::classify(queue.scope().is_some(), action.as_ref()),
            CallbackDisposition::Queued
        ));
        let duplicate = queue.callback("bound", &request);
        assert!(matches!(
            CallbackDisposition::classify(true, duplicate.as_ref()),
            CallbackDisposition::Rejected
        ));
        let drained = queue.drain(&scope()).unwrap();
        assert_eq!(drained.len(), 1);
        let revoked = queue.revoke(&scope()).unwrap();
        assert_eq!(revoked.len(), 1);
        assert_eq!(revoked[0].0, "bound");
        assert!(
            queue
                .complete(
                    &scope(),
                    ManagedToolResponse {
                        version: MANAGED_TOOL_BRIDGE_VERSION,
                        request_id: drained[0].request_id.clone(),
                        outcome: ManagedToolOutcome::Catalog {
                            tools: vec![],
                            next: None
                        },
                    }
                )
                .is_err()
        );
        let action = queue.callback("after-revoke", &request);
        assert!(matches!(
            CallbackDisposition::classify(false, action.as_ref()),
            CallbackDisposition::NoScope
        ));
        assert!(queue.queued.is_empty());
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn first_bind_catalog_is_correlated_and_revocation_restores_empty_catalog() {
        let mut queue = Queue::default();
        assert!(queue.bind(scope()));
        let request = CallbackRequest::McpMessage {
            server_name: SERVER.into(),
            message: json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        };
        assert!(matches!(
            queue.callback("bound-catalog", &request),
            Some(CallbackAction::Queued)
        ));
        let requests = queue.drain(&scope()).unwrap();
        assert!(matches!(
            requests[0].operation,
            ManagedToolOperation::Catalog { .. }
        ));
        let (callback_id, response) = queue.complete(&scope(), ManagedToolResponse {
            version: MANAGED_TOOL_BRIDGE_VERSION,
            request_id: requests[0].request_id.clone(),
            outcome: ManagedToolOutcome::Catalog { tools: vec![json!({
                "name":"filesystem.read", "description":"Read a file",
                "input_schema":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}},
            })], next: None },
        }).unwrap().unwrap();
        assert_eq!(callback_id, "bound-catalog");
        let CallbackResponse::Mcp { response } = response else {
            panic!("MCP response required");
        };
        assert_eq!(response["id"], 2);
        assert_eq!(response["result"]["tools"][0]["name"], "filesystem.read");
        assert_eq!(
            response["result"]["tools"][0]["inputSchema"]["required"],
            json!(["path"])
        );
        assert!(queue.pending.is_empty());
        assert!(queue.queued.is_empty());
        queue.revoke(&scope()).unwrap();
        let Some(CallbackAction::Reply(CallbackResponse::Mcp { response })) =
            queue.callback("revoked-catalog", &request)
        else {
            panic!("revoked catalog stays healthy but inert");
        };
        assert_eq!(response["result"]["tools"], json!([]));
    }

    #[test]
    fn nested_call_only_queues_and_requires_correlated_preparation_then_host_result() {
        let mut queue = Queue::default();
        assert!(matches!(
            queue.callback("native", &callback()),
            Some(CallbackAction::Reply(_))
        ));
        assert!(queue.bind(scope()));
        assert!(matches!(
            queue.callback("native", &callback()),
            Some(CallbackAction::Queued)
        ));
        let prepare = queue.drain(&scope()).unwrap().remove(0);
        let ManagedToolOperation::Prepare { invocation_id, .. } = prepare.operation else {
            panic!("prepare")
        };
        assert!(
            queue
                .complete(
                    &scope(),
                    ManagedToolResponse {
                        version: 1,
                        request_id: "forged".into(),
                        outcome: ManagedToolOutcome::Denied
                    }
                )
                .is_err()
        );
        assert!(
            queue
                .complete(
                    &scope(),
                    ManagedToolResponse {
                        version: 1,
                        request_id: prepare.request_id.clone(),
                        outcome: ManagedToolOutcome::Prepared {
                            invocation_id,
                            preparation_sha256: "a".repeat(64)
                        }
                    }
                )
                .unwrap()
                .is_none()
        );
        let invoke = queue.drain(&scope()).unwrap().remove(0);
        assert!(matches!(
            invoke.operation,
            ManagedToolOperation::Invoke { .. }
        ));
        let response = ManagedToolResponse {
            version: 1,
            request_id: invoke.request_id,
            outcome: ManagedToolOutcome::Recorded {
                result: json!({"model_output":"canonical","is_error":false}),
            },
        };
        assert_eq!(
            queue
                .complete(&scope(), response.clone())
                .unwrap()
                .unwrap()
                .0,
            "native"
        );
        assert!(queue.complete(&scope(), response).is_err());
        assert_eq!(queue.drain(&scope()).unwrap(), Vec::new());
    }
    #[test]
    fn stale_generation_revocation_and_denial_never_queue_execution() {
        let mut queue = Queue::default();
        queue.bind(scope());
        let _ = queue.callback("native", &callback());
        let prepare = queue.drain(&scope()).unwrap().remove(0);
        let mut stale = scope();
        stale.generation += 1;
        assert!(queue.drain(&stale).is_none());
        assert!(
            queue
                .complete(
                    &stale,
                    ManagedToolResponse {
                        version: 1,
                        request_id: prepare.request_id.clone(),
                        outcome: ManagedToolOutcome::Denied
                    }
                )
                .is_err()
        );
        assert!(
            queue
                .complete(
                    &scope(),
                    ManagedToolResponse {
                        version: 1,
                        request_id: prepare.request_id,
                        outcome: ManagedToolOutcome::Denied
                    }
                )
                .unwrap()
                .is_some()
        );
        assert_eq!(queue.drain(&scope()).unwrap(), Vec::new());
        let _ = queue.callback("next", &callback());
        assert_eq!(queue.revoke(&scope()).unwrap().len(), 1);
        assert!(matches!(
            queue.callback("after", &callback()),
            Some(CallbackAction::Reply(_))
        ));
    }
}
