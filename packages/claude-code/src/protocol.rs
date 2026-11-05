use crate::Error;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Caller attests exact version was behaviorally tested for native state markers.
/// Unknown modes can stream but can never report authoritative settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementSupport {
    Unknown,
    VerifiedSessionState,
}
/// Native state markers are not Bcode assignment outcomes.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Running,
    RequiresAction,
    Idle,
}
/// Conservative current transport observation, not a durable assignment verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settlement {
    Unsupported,
    Running,
    RequiresAction,
    Settled,
    Uncertain,
}
/// Native permission modes supported without bypassing approval policy.
#[derive(Clone, Copy, Debug, Serialize)]
pub enum PermissionMode {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "plan")]
    Plan,
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
}
/// Outbound control operations. Arbitrary command/flag injection is not supported.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlRequest {
    Initialize {
        hooks: Option<BTreeMap<String, Vec<HookMatcher>>>,
    },
    Interrupt,
    SetPermissionMode {
        mode: PermissionMode,
    },
    SetModel {
        model: String,
    },
}
/// Hook registrations are supplied before initialization; callbacks are delivered independently.
#[derive(Clone, Debug, Serialize)]
pub struct HookMatcher {
    pub matcher: Option<String>,
    #[serde(rename = "hookCallbackIds")]
    pub callback_ids: Vec<String>,
}
/// Bounded native callbacks. `AskUserQuestion` is a `CanUseTool` request, not a Bcode tool.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum CallbackRequest {
    CanUseTool {
        tool_name: String,
        input: Value,
        tool_use_id: Option<String>,
    },
    HookCallback {
        callback_id: String,
        input: Value,
        tool_use_id: Option<String>,
    },
    McpMessage {
        server_name: String,
        message: Value,
    },
}
/// Callback responses are matched against the original callback variant.
#[derive(Clone, Debug)]
pub enum CallbackResponse {
    Allow { updated_input: Value },
    Deny { message: String, interrupt: bool },
    Hook { output: Value },
    Mcp { response: Value },
    Error,
}
impl CallbackResponse {
    pub(crate) fn payload(self, request: &CallbackRequest) -> Result<Value, Error> {
        use serde_json::json;
        match (self, request) {
            (Self::Allow { updated_input }, CallbackRequest::CanUseTool { .. }) => {
                Ok(json!({"behavior":"allow","updatedInput":updated_input}))
            }
            (Self::Deny { message, interrupt }, CallbackRequest::CanUseTool { .. }) => {
                Ok(json!({"behavior":"deny","message":message,"interrupt":interrupt}))
            }
            (Self::Hook { output }, CallbackRequest::HookCallback { .. }) => Ok(output),
            (Self::Mcp { response }, CallbackRequest::McpMessage { .. }) => {
                Ok(json!({"mcp_response":response}))
            }
            _ => Err(Error::Protocol),
        }
    }
}
/// Raw native message bodies are ephemeral, bounded and potentially secret-bearing.
/// Consumers must normalize/redact before persistence, tracing or model context.
#[derive(Clone, Debug)]
pub enum NativeMessage {
    System(Value),
    Assistant(Value),
    User(Value),
    Result(Value),
    Stream(Value),
}
/// A bounded stream item; callback IDs are valid only in this process generation.
#[derive(Clone, Debug)]
pub enum Event {
    Message(NativeMessage),
    Callback {
        request_id: String,
        request: CallbackRequest,
    },
    CallbackCancelled {
        request_id: String,
    },
    Settlement(Settlement),
}

pub struct Lifecycle {
    support: SettlementSupport,
    state: Option<SessionState>,
    result: bool,
    submitted: bool,
    tasks: BTreeSet<String>,
    max_tasks: usize,
    uncertain: bool,
}
impl Lifecycle {
    pub(crate) const fn new(support: SettlementSupport, max_tasks: usize) -> Self {
        Self {
            support,
            state: None,
            result: false,
            submitted: false,
            tasks: BTreeSet::new(),
            max_tasks,
            uncertain: false,
        }
    }
    pub(crate) fn can_submit(&self) -> bool {
        !self.submitted || self.snapshot() == Settlement::Settled
    }
    pub(crate) const fn submit(&mut self) {
        self.submitted = true;
        self.result = false;
        self.state = Some(SessionState::Running);
    }
    pub(crate) const fn attention(&mut self) {
        self.state = Some(SessionState::RequiresAction);
    }
    pub(crate) const fn disconnect(&mut self) {
        self.uncertain = true;
    }
    pub(crate) fn snapshot(&self) -> Settlement {
        if self.uncertain {
            return Settlement::Uncertain;
        }
        if self.support == SettlementSupport::Unknown {
            return Settlement::Unsupported;
        }
        match self.state {
            Some(SessionState::RequiresAction) => Settlement::RequiresAction,
            Some(SessionState::Idle) if self.result && self.tasks.is_empty() => Settlement::Settled,
            _ => Settlement::Running,
        }
    }
    pub(crate) fn observe(&mut self, value: &Value) -> Result<(), Error> {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(Error::Protocol)?;
        if matches!(kind, "assistant" | "stream_event")
            && value.get("parent_tool_use_id").is_none_or(Value::is_null)
        {
            self.submit();
        }
        if kind == "result" {
            // Preserve current-activity idle: supported hosts can emit it just before result.
            self.result = true;
        }
        if kind != "system" {
            return Ok(());
        }
        match value.get("subtype").and_then(Value::as_str) {
            Some("session_state_changed") => {
                let state = value.get("state").ok_or(Error::Protocol)?.clone();
                self.state = Some(serde_json::from_value(state).map_err(|_| Error::Protocol)?);
            }
            Some("task_started") => {
                if !matches!(
                    value.get("task_type").and_then(Value::as_str),
                    Some("local_agent" | "local_workflow")
                ) {
                    // Unknown/background task classes cannot establish authoritative settlement.
                    self.support = SettlementSupport::Unknown;
                }
                let id = value
                    .get("task_id")
                    .and_then(Value::as_str)
                    .ok_or(Error::Protocol)?;
                crate::launch::validate_id(id)?;
                if !self.tasks.contains(id) && self.tasks.len() >= self.max_tasks {
                    return Err(Error::Capacity);
                }
                self.tasks.insert(id.to_owned());
                if self.state == Some(SessionState::Idle) {
                    self.state = Some(SessionState::Running);
                }
            }
            Some("task_notification" | "task_updated") => {
                let status = value
                    .get("status")
                    .or_else(|| value.pointer("/patch/status"))
                    .and_then(Value::as_str);
                if matches!(status, Some("completed" | "failed" | "stopped" | "killed")) {
                    let id = value
                        .get("task_id")
                        .and_then(Value::as_str)
                        .ok_or(Error::Protocol)?;
                    if self.tasks.remove(id) {
                        // Completion can owe further work: invalidate prior idle, but do not
                        // invent a required extra result. A fresh authoritative idle decides.
                        self.state = Some(SessionState::Running);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Lifecycle, Settlement, SettlementSupport};
    use serde_json::json;

    #[test]
    fn idle_and_result_in_either_order_settle_only_current_activity() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::VerifiedSessionState, 2);
        lifecycle.submit();
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .expect("idle");
        assert_ne!(lifecycle.snapshot(), Settlement::Settled);
        lifecycle
            .observe(&json!({"type":"result"}))
            .expect("result");
        assert_eq!(lifecycle.snapshot(), Settlement::Settled);
        lifecycle.submit();
        lifecycle
            .observe(&json!({"type":"result"}))
            .expect("new result without new idle");
        assert_ne!(lifecycle.snapshot(), Settlement::Settled);
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .expect("idle");
        assert_eq!(lifecycle.snapshot(), Settlement::Settled);
    }

    #[test]
    fn killed_task_needs_fresh_idle_not_invented_result_and_unknown_classes_disable_settlement() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::VerifiedSessionState, 2);
        lifecycle.submit();
        lifecycle.observe(&json!({"type":"system","subtype":"task_started","task_id":"one","task_type":"local_agent"})).expect("start");
        lifecycle
            .observe(&json!({"type":"result"}))
            .expect("result");
        lifecycle.observe(&json!({"type":"system","subtype":"task_updated","task_id":"one","patch":{"status":"killed"}})).expect("terminal task");
        assert_ne!(lifecycle.snapshot(), Settlement::Settled);
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .expect("idle");
        assert_eq!(lifecycle.snapshot(), Settlement::Settled);
        lifecycle.observe(&json!({"type":"system","subtype":"task_notification","task_id":"one","status":"stopped"})).expect("duplicate terminal observation");
        assert_eq!(lifecycle.snapshot(), Settlement::Settled);
        lifecycle.observe(&json!({"type":"system","subtype":"task_started","task_id":"two","task_type":"future_task"})).expect("unknown class");
        assert_eq!(lifecycle.snapshot(), Settlement::Unsupported);
    }
}
