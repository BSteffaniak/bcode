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
    /// Native lifecycle/rate-limit metadata; neither a turn nor settlement proof.
    Notice(Value),
}
/// A bounded stream item; callback IDs are valid only in this process generation.
#[derive(Clone, Debug)]
pub enum Event {
    Message(NativeMessage),
    /// Native consumed-prompt evidence; never a native turn ID or settlement proof.
    Attribution(crate::SubmissionAttribution),
    Callback {
        request_id: String,
        request: CallbackRequest,
    },
    CallbackCancelled {
        request_id: String,
    },
    Settlement(Settlement),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BackgroundContinuation {
    NotObserved,
    Unresolved,
}

pub struct Lifecycle {
    support: SettlementSupport,
    state: Option<SessionState>,
    result: bool,
    submitted: bool,
    tasks: BTreeSet<String>,
    background_snapshot: BTreeSet<String>,
    max_tasks: usize,
    uncertain: bool,
    background_continuation: BackgroundContinuation,
    queued_user_sends: Option<u64>,
}
impl Lifecycle {
    pub(crate) const fn new(support: SettlementSupport, max_tasks: usize) -> Self {
        Self {
            support,
            state: None,
            result: false,
            submitted: false,
            tasks: BTreeSet::new(),
            background_snapshot: BTreeSet::new(),
            max_tasks,
            uncertain: false,
            background_continuation: BackgroundContinuation::NotObserved,
            queued_user_sends: None,
        }
    }
    /// Foreground turn admission, not settlement. The previous foreground result must
    /// have arrived with nothing queued; background work continues as observations.
    /// Verified mode additionally waits for the native idle marker.
    pub(crate) fn can_submit(&self) -> bool {
        if !self.submitted {
            return true;
        }
        if self.uncertain
            || !self.result
            || self.state == Some(SessionState::RequiresAction)
            || self.queued_user_sends.is_some_and(|count| count != 0)
        {
            return false;
        }
        self.support == SettlementSupport::Unknown || self.state == Some(SessionState::Idle)
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
            Some(SessionState::Idle)
                if self.result
                    && self.tasks.is_empty()
                    && self.background_snapshot.is_empty()
                    && self.background_continuation == BackgroundContinuation::NotObserved
                    && self.queued_user_sends.is_none_or(|count| count == 0) =>
            {
                Settlement::Settled
            }
            _ => Settlement::Running,
        }
    }
    fn observe_background_tool_intent(&mut self, value: &Value, kind: &str) -> Result<(), Error> {
        if kind == "assistant"
            && value.get("parent_tool_use_id").is_none_or(Value::is_null)
            && let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array)
        {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && block.get("name").and_then(Value::as_str) == Some("Bash")
                    && let Some(background) = block.pointer("/input/run_in_background")
                    && background.as_bool().ok_or(Error::Protocol)?
                {
                    // The legacy aggregate settlement path cannot identify all wake
                    // turns. Keep it running; the independent execution tracker owns
                    // exact task/continuation terminals and coverage.
                    self.background_continuation = BackgroundContinuation::Unresolved;
                }
            }
        }
        Ok(())
    }
    fn observe_background_snapshot(&mut self, value: &Value) -> Result<(), Error> {
        let entries = value
            .get("tasks")
            .and_then(Value::as_array)
            .ok_or(Error::Protocol)?;
        if entries.len() > self.max_tasks {
            // Native inventory remains available to the paged adapter. The legacy
            // aggregate settlement cache cannot cover it; do not kill the reader.
            self.support = SettlementSupport::Unknown;
            self.state = Some(SessionState::Running);
            return Ok(());
        }
        let mut snapshot = BTreeSet::new();
        let mut unknown = false;
        for entry in entries {
            let id = entry
                .get("task_id")
                .and_then(Value::as_str)
                .ok_or(Error::Protocol)?;
            crate::launch::validate_id(id)?;
            if !snapshot.insert(id.to_owned()) {
                return Err(Error::Protocol);
            }
            let task_type = entry
                .get("task_type")
                .and_then(Value::as_str)
                .ok_or(Error::Protocol)?;
            if task_type == "local_bash" {
                self.background_continuation = BackgroundContinuation::Unresolved;
            }
            unknown |= !matches!(task_type, "local_agent" | "local_workflow" | "local_bash");
        }
        if unknown {
            self.support = SettlementSupport::Unknown;
        }
        // Replace only the background snapshot, never independently observed tasks.
        // Completion/removal can owe a follow-up: require a fresh authoritative idle.
        if snapshot != self.background_snapshot {
            self.state = Some(SessionState::Running);
        }
        self.background_snapshot = snapshot;
        Ok(())
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
        self.observe_background_tool_intent(value, kind)?;
        if kind == "user"
            && value.get("parent_tool_use_id").is_none_or(Value::is_null)
            && let Some(id) = value
                .pointer("/tool_use_result/backgroundTaskId")
                .or_else(|| value.pointer("/tool_use_result/background_task_id"))
        {
            crate::launch::validate_id(id.as_str().ok_or(Error::Protocol)?)?;
            self.background_continuation = BackgroundContinuation::Unresolved;
        }
        if kind == "result" {
            // Preserve current-activity idle: supported hosts can emit it just before result.
            self.result = true;
            self.queued_user_sends = value
                .get("queued_turn_count")
                .map(|count| count.as_u64().ok_or(Error::Protocol))
                .transpose()?;
        }
        if kind != "system" {
            return Ok(());
        }
        match value.get("subtype").and_then(Value::as_str) {
            Some("session_state_changed") => {
                let state = value.get("state").ok_or(Error::Protocol)?.clone();
                self.state = Some(serde_json::from_value(state).map_err(|_| Error::Protocol)?);
            }
            Some("background_tasks_changed") => self.observe_background_snapshot(value)?,
            Some("task_started") => {
                if value.get("task_type").and_then(Value::as_str) == Some("local_bash") {
                    self.background_continuation = BackgroundContinuation::Unresolved;
                }
                if !matches!(
                    value.get("task_type").and_then(Value::as_str),
                    Some("local_agent" | "local_workflow" | "local_bash")
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
                    // The streaming adapter can page these native records to the
                    // canonical host; only legacy aggregate settlement is unknown.
                    self.support = SettlementSupport::Unknown;
                    self.state = Some(SessionState::Running);
                    return Ok(());
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
    fn supported_background_shell_is_tracked_without_claiming_aggregate_settlement() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::VerifiedSessionState, 4);
        lifecycle.submit();
        lifecycle.observe(&json!({"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"shell","task_type":"local_bash"}]})).unwrap();
        lifecycle
            .observe(&json!({"type":"result","queued_turn_count":0}))
            .unwrap();
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .unwrap();
        assert_eq!(lifecycle.snapshot(), Settlement::Running);
        assert!(lifecycle.can_submit());
        lifecycle
            .observe(&json!({"type":"system","subtype":"background_tasks_changed","tasks":[]}))
            .unwrap();
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .unwrap();
        assert_eq!(lifecycle.snapshot(), Settlement::Running);
    }

    #[test]
    fn snapshots_block_idle_and_replacement_requires_fresh_idle() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::VerifiedSessionState, 2);
        lifecycle.submit();
        lifecycle
            .observe(
                &json!({"type":"system","subtype":"background_tasks_changed",
            "tasks":[{"task_id":"agent-1","task_type":"local_agent"}]}),
            )
            .unwrap();
        lifecycle.observe(&json!({"type":"result"})).unwrap();
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .unwrap();
        assert_ne!(lifecycle.snapshot(), Settlement::Settled);
        lifecycle
            .observe(&json!({"type":"system","subtype":"background_tasks_changed","tasks":[]}))
            .unwrap();
        assert_ne!(lifecycle.snapshot(), Settlement::Settled);
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .unwrap();
        assert_eq!(lifecycle.snapshot(), Settlement::Settled);
    }

    #[test]
    fn oversized_legacy_inventory_is_unknown_but_reader_keeps_native_evidence() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::VerifiedSessionState, 1);
        lifecycle.submit();
        lifecycle.observe(&json!({"type":"system","subtype":"background_tasks_changed","tasks":[
            {"task_id":"a","task_type":"local_bash"},{"task_id":"b","task_type":"local_bash"}
        ]})).unwrap();
        assert_eq!(lifecycle.snapshot(), Settlement::Unsupported);
        for id in ["a", "b"] {
            lifecycle.observe(&json!({"type":"system","subtype":"task_started","task_id":id,"task_type":"local_bash"})).unwrap();
        }
        assert_eq!(lifecycle.snapshot(), Settlement::Unsupported);
    }

    #[test]
    fn malformed_snapshots_and_background_shell_result_fail_closed() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::VerifiedSessionState, 1);
        for tasks in [json!(null), json!([{}]), json!([{"task_id":"x"}])] {
            assert!(
                lifecycle
                    .observe(
                        &json!({"type":"system","subtype":"background_tasks_changed","tasks":tasks})
                    )
                    .is_err()
            );
        }
        let mut duplicates = Lifecycle::new(SettlementSupport::VerifiedSessionState, 2);
        assert!(duplicates.observe(&json!({"type":"system","subtype":"background_tasks_changed",
            "tasks":[{"task_id":"x","task_type":"local_agent"},{"task_id":"x","task_type":"local_agent"}]})).is_err());
        lifecycle
            .observe(&json!({"type":"user","tool_use_result":{"backgroundTaskId":"shell-1"}}))
            .unwrap();
        lifecycle.observe(&json!({"type":"result"})).unwrap();
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .unwrap();
        assert_eq!(lifecycle.snapshot(), Settlement::Running);
    }

    #[test]
    fn empty_snapshot_never_erases_an_independently_started_task() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::VerifiedSessionState, 2);
        lifecycle.submit();
        lifecycle.observe(&json!({"type":"system","subtype":"task_started","task_id":"foreground","task_type":"local_agent"})).unwrap();
        lifecycle
            .observe(&json!({"type":"system","subtype":"background_tasks_changed","tasks":[]}))
            .unwrap();
        lifecycle.observe(&json!({"type":"result"})).unwrap();
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .unwrap();
        assert_ne!(lifecycle.snapshot(), Settlement::Settled);
    }

    #[test]
    fn background_tool_intent_fences_idle_even_without_task_started() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::VerifiedSessionState, 2);
        lifecycle.submit();
        lifecycle
            .observe(&json!({"type":"assistant","message":{"content":[{
                "type":"tool_use","id":"tool-1","name":"Bash",
                "input":{"command":"sleep 3","run_in_background":true}
            }]}}))
            .unwrap();
        lifecycle
            .observe(&json!({"type":"result","queued_turn_count":0}))
            .unwrap();
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .unwrap();
        assert_eq!(lifecycle.snapshot(), Settlement::Running);
    }

    #[test]
    fn queued_sends_block_settlement_and_background_bash_idle_is_not_completion() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::VerifiedSessionState, 2);
        lifecycle.submit();
        lifecycle
            .observe(&json!({"type":"result","queued_turn_count":1}))
            .unwrap();
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .unwrap();
        assert_ne!(lifecycle.snapshot(), Settlement::Settled);
        assert!(
            lifecycle
                .observe(&json!({"type":"result","queued_turn_count":-1}))
                .is_err()
        );

        let mut background = Lifecycle::new(SettlementSupport::VerifiedSessionState, 2);
        background.submit();
        background.observe(&json!({"type":"system","subtype":"task_started","task_id":"bash-1","task_type":"local_bash"})).unwrap();
        background
            .observe(&json!({"type":"result","queued_turn_count":0}))
            .unwrap();
        background
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .unwrap();
        assert_eq!(background.snapshot(), Settlement::Running);
    }

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

    #[test]
    fn unknown_support_admits_next_foreground_turn_after_result_without_settlement() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::Unknown, 2);
        assert!(lifecycle.can_submit());
        lifecycle.submit();
        assert!(!lifecycle.can_submit(), "running turn blocks new input");
        lifecycle
            .observe(&json!({"type":"result","queued_turn_count":1}))
            .expect("result");
        assert!(
            !lifecycle.can_submit(),
            "queued native input blocks new input"
        );
        lifecycle
            .observe(&json!({"type":"result","queued_turn_count":0}))
            .expect("result");
        assert!(lifecycle.can_submit());
        assert_eq!(lifecycle.snapshot(), Settlement::Unsupported);
        lifecycle
            .observe(&json!({"type":"assistant","parent_tool_use_id":null}))
            .expect("automatic continuation");
        assert!(
            !lifecycle.can_submit(),
            "native continuation blocks until its result"
        );
        lifecycle
            .observe(&json!({"type":"result"}))
            .expect("result");
        assert!(lifecycle.can_submit());
        lifecycle.disconnect();
        assert!(!lifecycle.can_submit());
    }

    #[test]
    fn downgraded_verified_session_still_admits_foreground_turns() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::VerifiedSessionState, 2);
        lifecycle.submit();
        lifecycle.observe(&json!({"type":"system","subtype":"task_started","task_id":"x","task_type":"future_task"})).expect("unknown class");
        lifecycle
            .observe(&json!({"type":"result"}))
            .expect("result");
        assert_eq!(lifecycle.snapshot(), Settlement::Unsupported);
        assert!(lifecycle.can_submit());
    }

    #[test]
    fn verified_support_still_waits_for_idle_and_attention_blocks() {
        let mut lifecycle = Lifecycle::new(SettlementSupport::VerifiedSessionState, 2);
        lifecycle.submit();
        lifecycle
            .observe(&json!({"type":"result"}))
            .expect("result");
        assert!(!lifecycle.can_submit());
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"requires_action"}))
            .expect("attention");
        assert!(!lifecycle.can_submit());
        lifecycle
            .observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .expect("idle");
        assert!(lifecycle.can_submit());
    }
}
