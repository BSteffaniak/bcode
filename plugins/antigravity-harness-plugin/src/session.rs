//! Antigravity ACP session state machine.
//!
//! Translates `bcode.harness/v3` operations onto a single `agy acp --stdio --disable-telemetry`
//! process bound to one confirmed ACP `sessionId`.
use crate::display::{Display, bounded_id, extract_chunk_text, format_permission_preview};
use crate::retention::Retention;
use crate::transport::{AcpError, AcpServer, InboundEvent, Launch, Readiness, Wake};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessActivity, HarnessAttemptIdentity, HarnessAttention,
    HarnessChoice, HarnessCompleteness, HarnessConnectivity, HarnessContinuation,
    HarnessDeliveryState, HarnessExecutionEvent, HarnessExecutionStatus, HarnessHydrationRequest,
    HarnessInteraction, HarnessInteractionKind, HarnessInventoryPage, HarnessLiveStream,
    HarnessLiveUpdate, HarnessObservation, HarnessObservationKind, HarnessPermissionMode,
    HarnessPlacement, HarnessReceipt, HarnessResponse, HarnessResponseValue,
    HarnessRetentionContext, HarnessRetentionPage, HarnessSessionBinding, HarnessSessionSetting,
    HarnessSubmission, HarnessTerminalDisposition, MAX_HARNESS_ITEMS,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const MAX_RETAINED_SUBMISSIONS: usize = 64;
const MAX_RETAINED_RESPONSES: usize = 64;

struct ActiveTurn {
    attempt: HarnessAttemptIdentity,
    rpc_id: u64,
    interrupted: bool,
}

struct PendingPermission {
    rpc_id: Value,
    interaction: HarnessInteraction,
    allow_option_id: Option<String>,
    deny_option_id: Option<String>,
}

/// Live Antigravity ACP session bound to one canonical Bcode session and generation.
pub struct AcpSession {
    server: AcpServer,
    binding: HarnessSessionBinding,
    display: Display,
    retention: Retention,
    staged: VecDeque<HarnessObservation>,
    observation_seq: u64,
    active_turn: Option<ActiveTurn>,
    submissions: BTreeMap<String, (HarnessSubmission, HarnessReceipt)>,
    pending_permissions: BTreeMap<String, PendingPermission>,
    responses: BTreeMap<String, HarnessResponse>,
    reported_notices: BTreeSet<&'static str>,
    reported_disconnect: bool,
    gap_pending: bool,
    fenced: bool,
}

impl AcpSession {
    /// Spawn `agy acp`, initialize, and either create (`session/new`) or resume (`session/resume`)
    /// the exact native session. Optionally applies an exact selected model before returning.
    ///
    /// # Errors
    /// * Fails closed on protocol mismatch, version mismatch on resume, auth failure, or transport
    ///   errors.
    pub fn open_with_model(
        launch: &Launch,
        mut binding: HarnessSessionBinding,
        resume: bool,
        selected_model: Option<&str>,
    ) -> Result<Self, AcpError> {
        let mut server = AcpServer::spawn(launch)?;
        let native_version = server.initialize(launch)?;
        if resume && binding.native_version != native_version {
            return Err(AcpError::Unsupported);
        }
        let cwd = launch
            .working_directory
            .to_str()
            .ok_or(AcpError::Configuration)?;

        let open_result = if resume {
            server.call(
                "session/resume",
                &json!({
                    "sessionId": binding.native_session_id,
                    "cwd": cwd,
                    "mcpServers": [],
                }),
            )?
        } else {
            server.call(
                "session/new",
                &json!({
                    "cwd": cwd,
                    "mcpServers": [],
                }),
            )?
        };

        let native_session_id =
            bounded_id(open_result.get("sessionId")).ok_or(AcpError::Protocol)?;
        if resume && native_session_id != binding.native_session_id {
            return Err(AcpError::Protocol);
        }

        binding.schema_version = HARNESS_SCHEMA_VERSION;
        binding.native_session_id = native_session_id;
        binding.native_version = native_version;

        let mut display = Display::default();
        let current_model = open_result
            .get("models")
            .and_then(|m| m.get("currentModelId"))
            .and_then(Value::as_str);
        display.set_model(current_model);

        if let Some(model_id) = selected_model {
            server.call(
                "session/set_model",
                &json!({
                    "sessionId": binding.native_session_id,
                    "modelId": model_id,
                }),
            )?;
            display.set_model(Some(model_id));
        }

        Ok(Self {
            server,
            binding,
            display,
            retention: Retention::default(),
            staged: VecDeque::new(),
            observation_seq: 0,
            active_turn: None,
            submissions: BTreeMap::new(),
            pending_permissions: BTreeMap::new(),
            responses: BTreeMap::new(),
            reported_notices: BTreeSet::new(),
            reported_disconnect: false,
            gap_pending: false,
            fenced: false,
        })
    }

    /// Confirmed native session binding.
    #[must_use]
    pub const fn binding(&self) -> &HarnessSessionBinding {
        &self.binding
    }

    /// Current native model id, if known.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.display.model()
    }

    /// Physical process reap flag owned by the transport supervisor.
    #[must_use]
    pub fn reap_flag(&self) -> Arc<AtomicBool> {
        self.server.reap_flag()
    }

    /// Wake handle for waiting `observe` invocations.
    #[must_use]
    pub fn transport_wake(&self) -> Wake {
        self.server.wake()
    }

    /// Readiness of the underlying transport queue.
    #[must_use]
    pub fn transport_readiness(&self) -> Readiness {
        self.server.readiness()
    }

    /// Whether durable observations or a disconnect/gap are already staged locally.
    #[must_use]
    pub fn locally_ready(&self) -> bool {
        !self.staged.is_empty()
            || self.gap_pending
            || (self.server.is_closed() && !self.reported_disconnect)
            || self.display.live_near_capacity()
    }

    /// Whether transient live updates are queued.
    #[must_use]
    pub fn live_pending(&self) -> bool {
        self.display.live_pending()
    }

    /// Take pending transient live updates for one observation page.
    pub fn take_live(&mut self) -> Vec<HarnessLiveUpdate> {
        self.display.take_live()
    }

    /// Apply retention acknowledgement/hydration context from the host.
    ///
    /// # Errors
    /// * Rejects stale/mismatched retention contexts.
    pub fn apply_retention(&mut self, context: &HarnessRetentionContext) -> Result<(), AcpError> {
        let _ = self
            .retention
            .apply(context, &self.binding.session_id, self.binding.generation)?;
        Ok(())
    }

    /// Mint a retention page descriptor for one delivered batch of observations.
    ///
    /// # Errors
    /// * Fails if a previous page is unacknowledged or bounds are exceeded.
    pub fn retention_page(
        &mut self,
        observations: &[HarnessObservation],
    ) -> Result<HarnessRetentionPage, AcpError> {
        self.retention.deliver(observations)
    }

    /// Pending hydration requests (none currently emitted by ACP).
    #[must_use]
    pub fn hydration_requests(&self) -> Vec<HarnessHydrationRequest> {
        self.retention.requests()
    }

    /// Optional background inventory page (unused by ACP).
    #[must_use]
    pub const fn take_inventory_page(&mut self) -> Option<HarnessInventoryPage> {
        None
    }

    /// Submit one user prompt as `NextTurn`.
    ///
    /// # Errors
    /// * Rejects steering, stale generation, busy turn, pending callbacks, or disconnected transport.
    pub fn submit(&mut self, submission: &HarnessSubmission) -> Result<HarnessReceipt, AcpError> {
        if submission.session_id != self.binding.session_id
            || submission.generation != self.binding.generation
        {
            return Err(AcpError::Stale);
        }
        if let Some((retained, receipt)) = self.submissions.get(&submission.submission_id) {
            return if retained == submission {
                Ok(receipt.clone())
            } else {
                Err(AcpError::Stale)
            };
        }
        if self.fenced || self.server.is_closed() {
            return Err(AcpError::Disconnected);
        }
        if !matches!(submission.placement, HarnessPlacement::NextTurn) {
            return Err(AcpError::Unsupported);
        }
        if self.active_turn.is_some() || !self.pending_permissions.is_empty() {
            return Err(AcpError::Rejected);
        }

        self.display.begin_turn();
        let rpc_id = self.server.send_async_request(
            "session/prompt",
            &json!({
                "sessionId": self.binding.native_session_id,
                "prompt": [
                    {
                        "type": "text",
                        "text": submission.text,
                    }
                ],
            }),
        )?;

        let receipt = HarnessReceipt {
            submission_id: submission.submission_id.clone(),
            attempt_id: submission.attempt_id.clone(),
            session_id: submission.session_id.clone(),
            generation: submission.generation,
            state: HarnessDeliveryState::DeliveryAttempted,
            native_request_id: Some(format!("acp-rpc-{rpc_id}")),
        };
        if self.submissions.len() >= MAX_RETAINED_SUBMISSIONS {
            self.submissions.pop_first();
        }
        self.submissions.insert(
            submission.submission_id.clone(),
            (submission.clone(), receipt.clone()),
        );
        self.active_turn = Some(ActiveTurn {
            attempt: HarnessAttemptIdentity {
                submission_id: submission.submission_id.clone(),
                attempt_id: submission.attempt_id.clone(),
            },
            rpc_id,
            interrupted: false,
        });
        Ok(receipt)
    }

    /// Answer an outstanding `session/request_permission` interaction.
    ///
    /// # Errors
    /// * Rejects unknown/withdrawn requests, wrong generation, or unsupported response kinds.
    pub fn respond(&mut self, response: &HarnessResponse) -> Result<(), AcpError> {
        if response.session_id != self.binding.session_id
            || response.generation != self.binding.generation
        {
            return Err(AcpError::Stale);
        }
        if let Some(retained) = self.responses.get(&response.response_id) {
            return if retained == response {
                Ok(())
            } else {
                Err(AcpError::Stale)
            };
        }
        if self.fenced || self.server.is_closed() {
            return Err(AcpError::Disconnected);
        }
        let pending = self
            .pending_permissions
            .get(&response.request_id)
            .ok_or(AcpError::Stale)?;
        response
            .validate_for(&pending.interaction)
            .map_err(|_| AcpError::Protocol)?;

        let outcome = match &response.value {
            HarnessResponseValue::Approval { approved: true } => {
                let option_id = pending
                    .allow_option_id
                    .as_deref()
                    .ok_or(AcpError::Unsupported)?;
                json!({
                    "outcome": {
                        "outcome": "selected",
                        "optionId": option_id,
                    }
                })
            }
            HarnessResponseValue::Approval { approved: false } => {
                pending.deny_option_id.as_deref().map_or_else(
                    || json!({ "outcome": { "outcome": "cancelled" } }),
                    |option_id| {
                        json!({
                            "outcome": {
                                "outcome": "selected",
                                "optionId": option_id,
                            }
                        })
                    },
                )
            }
            HarnessResponseValue::Cancel => {
                json!({ "outcome": { "outcome": "cancelled" } })
            }
            HarnessResponseValue::Answer { .. } | HarnessResponseValue::Assistance { .. } => {
                return Err(AcpError::Unsupported);
            }
        };

        let rpc_id = pending.rpc_id.clone();
        self.server.reply_result(&rpc_id, &outcome)?;
        self.pending_permissions.remove(&response.request_id);
        if self.responses.len() >= MAX_RETAINED_RESPONSES {
            self.responses.pop_first();
        }
        self.responses
            .insert(response.response_id.clone(), response.clone());
        Ok(())
    }

    /// Send ACP `session/cancel` for the currently active turn.
    ///
    /// # Errors
    /// * Rejects stale generation or disconnected transport.
    pub fn interrupt_active(&mut self, generation: u64) -> Result<(), AcpError> {
        if generation != self.binding.generation {
            return Err(AcpError::Stale);
        }
        if self.fenced || self.server.is_closed() {
            return Err(AcpError::Disconnected);
        }
        if let Some(turn) = self.active_turn.as_mut() {
            turn.interrupted = true;
        }
        self.server.notify(
            "session/cancel",
            &json!({
                "sessionId": self.binding.native_session_id,
            }),
        )
    }

    /// Apply a mid-session model or permission-mode change via ACP RPC.
    ///
    /// # Errors
    /// * Returns `Rejected` if the native agent refuses the setting, or `Disconnected` if closed.
    pub fn configure(&mut self, setting: &HarnessSessionSetting) -> Result<(), AcpError> {
        if self.fenced || self.server.is_closed() {
            return Err(AcpError::Disconnected);
        }
        match setting {
            HarnessSessionSetting::Model { model_id } => {
                self.server.call(
                    "session/set_model",
                    &json!({
                        "sessionId": self.binding.native_session_id,
                        "modelId": model_id,
                    }),
                )?;
                self.display.set_model(Some(model_id));
            }
            HarnessSessionSetting::PermissionMode { mode } => {
                let mode_id = match mode {
                    HarnessPermissionMode::Default => "default",
                    HarnessPermissionMode::AcceptEdits => "accept-edits",
                    HarnessPermissionMode::Plan => "plan",
                };
                self.server.call(
                    "session/set_mode",
                    &json!({
                        "sessionId": self.binding.native_session_id,
                        "modeId": mode_id,
                    }),
                )?;
            }
        }
        Ok(())
    }

    /// Drain inbound ACP events and return up to `limit` normalized observations.
    ///
    /// # Errors
    /// * Fails if retention backpressure forbids advancing.
    pub fn observe(
        &mut self,
        limit: usize,
    ) -> Result<(Vec<HarnessObservation>, HarnessCompleteness), AcpError> {
        if !self.retention.ready() {
            return Err(AcpError::Uncertain);
        }
        let max_items = limit.max(1);
        self.pump_transport(max_items * 4);

        let mut out = Vec::new();
        while out.len() < max_items {
            let Some(obs) = self.staged.pop_front() else {
                break;
            };
            out.push(obs);
        }

        let completeness = if self.gap_pending {
            self.gap_pending = false;
            HarnessCompleteness::Gap
        } else if self.staged.is_empty() {
            HarnessCompleteness::Complete
        } else {
            HarnessCompleteness::Partial
        };
        Ok((out, completeness))
    }

    /// Current orthogonal execution status snapshot.
    #[must_use]
    pub fn status(&self) -> HarnessExecutionStatus {
        let connectivity = if self.fenced || self.server.is_closed() {
            HarnessConnectivity::Disconnected
        } else {
            HarnessConnectivity::Connected
        };
        let activity = self.active_turn.as_ref().map_or_else(
            || {
                if connectivity == HarnessConnectivity::Connected {
                    HarnessActivity::Idle
                } else {
                    HarnessActivity::Unknown
                }
            },
            |turn| {
                if turn.interrupted {
                    HarnessActivity::Interrupted
                } else {
                    HarnessActivity::Running
                }
            },
        );
        let attention = if self.pending_permissions.is_empty() {
            HarnessAttention::None
        } else {
            HarnessAttention::Approval
        };
        HarnessExecutionStatus {
            connectivity,
            activity,
            attention,
            continuation: HarnessContinuation::Unknown,
            completeness: if self.gap_pending {
                HarnessCompleteness::Gap
            } else {
                HarnessCompleteness::Complete
            },
        }
    }

    /// Return current status for read-only reconciliation.
    ///
    /// # Errors
    /// * Never mutates native state.
    pub fn reconcile(&mut self) -> Result<HarnessExecutionStatus, AcpError> {
        self.pump_transport(16);
        Ok(self.status())
    }

    /// Gracefully send `session/close` before dropping the transport.
    pub fn send_close(&mut self) {
        if !self.server.is_closed() {
            let _ = self.server.notify(
                "session/close",
                &json!({
                    "sessionId": self.binding.native_session_id,
                }),
            );
        }
        self.server.close();
    }

    fn pump_transport(&mut self, max_events: usize) {
        let events = self.server.drain_events(max_events);
        for event in events {
            match event {
                InboundEvent::Notification { method, params } => {
                    self.handle_notification(&method, &params);
                }
                InboundEvent::ServerRequest { id, method, params } => {
                    self.handle_server_request(id, &method, &params);
                }
                InboundEvent::AsyncReply { id, message } => {
                    self.handle_async_reply(id, &message);
                }
                InboundEvent::MalformedFrame(code) => {
                    self.stage_notice_once(code, "Skipped malformed native ACP frame");
                }
            }
        }

        if self.server.is_overflowed() && !self.fenced {
            self.fenced = true;
            self.gap_pending = true;
            self.withdraw_all_permissions();
            self.display.clear();
            self.stage_notice_once(
                "native_queue_overflow",
                "Antigravity output exceeded bounded queue; transport closed",
            );
        }

        if self.server.is_closed() && !self.reported_disconnect {
            self.reported_disconnect = true;
            self.withdraw_all_permissions();
            if let Some(flushed) = self.display.flush_active() {
                self.push_observation(flushed, Vec::new());
            }
            self.display.clear();
            let status = self.status();
            self.push_observation(HarnessObservationKind::Status { status }, Vec::new());
        }
    }

    fn handle_notification(&mut self, method: &str, params: &Value) {
        if method != "session/update" {
            return;
        }
        if let Some(sid) = params.get("sessionId").and_then(Value::as_str)
            && sid != self.binding.native_session_id
        {
            // Native session identity mismatch fences the transport immediately.
            self.fenced = true;
            self.gap_pending = true;
            self.server.close();
            self.stage_notice_once(
                "native_session_mismatch",
                "Antigravity reported an unexpected session identity",
            );
            return;
        }
        let Some(update) = params.get("update") else {
            return;
        };
        let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
            return;
        };
        match kind {
            "agent_message_chunk" => {
                if let Some(text) = extract_chunk_text(update)
                    && let Some(flushed) = self.display.push_chunk(HarnessLiveStream::Text, text)
                {
                    self.push_observation(flushed, Vec::new());
                }
            }
            "agent_thought_chunk" => {
                if let Some(text) = extract_chunk_text(update)
                    && let Some(flushed) =
                        self.display.push_chunk(HarnessLiveStream::Reasoning, text)
                {
                    self.push_observation(flushed, Vec::new());
                }
            }
            "tool_call" | "tool_call_update" => {
                let observations = self.display.project_tool_update(update);
                for obs in observations {
                    self.push_observation(obs, Vec::new());
                }
            }
            "config_option_update" => {
                if let Some(options) = update.get("configOptions").and_then(Value::as_array) {
                    for opt in options {
                        if opt.get("id").and_then(Value::as_str) == Some("model")
                            && let Some(val) = opt.get("currentValue").and_then(Value::as_str)
                        {
                            self.display.set_model(Some(val));
                        }
                    }
                }
            }
            // Routine/unrecognized ACP updates that do not need transcript items.
            _ => {}
        }
    }

    fn handle_server_request(&mut self, id: Value, method: &str, params: &Value) {
        if method != "session/request_permission" {
            let _ = self
                .server
                .reply_error(&id, -32601, "Unsupported client method");
            return;
        }
        if let Some(sid) = params.get("sessionId").and_then(Value::as_str)
            && sid != self.binding.native_session_id
        {
            let _ = self
                .server
                .reply_error(&id, -32602, "Mismatched session identity");
            return;
        }
        if let Some(flushed) = self.display.flush_active() {
            self.push_observation(flushed, Vec::new());
        }

        let tool_call = params.get("toolCall").unwrap_or(&Value::Null);
        let tool_name =
            bounded_id(tool_call.get("name")).or_else(|| bounded_id(tool_call.get("kind")));
        let input_preview = format_permission_preview(tool_call);
        let title = tool_call
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Native tool permission");

        let mut allow_option_id = None;
        let mut deny_option_id = None;
        let mut choices = Vec::new();
        if let Some(options) = params.get("options").and_then(Value::as_array) {
            for opt in options.iter().take(MAX_HARNESS_ITEMS) {
                let Some(option_id) = bounded_id(opt.get("optionId")) else {
                    continue;
                };
                let label = opt
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&option_id)
                    .to_owned();
                let kind = opt.get("kind").and_then(Value::as_str).unwrap_or_default();
                if allow_option_id.is_none()
                    && (kind == "allow_once" || option_id.eq_ignore_ascii_case("allow"))
                {
                    allow_option_id = Some(option_id.clone());
                } else if deny_option_id.is_none()
                    && (kind == "reject_once" || option_id.eq_ignore_ascii_case("deny"))
                {
                    deny_option_id = Some(option_id.clone());
                }
                choices.push(HarnessChoice {
                    choice_id: option_id,
                    label,
                    description: None,
                });
            }
        }

        let request_id = format!("acp-perm-{}", uuid::Uuid::new_v4());
        let interaction = HarnessInteraction {
            request_id: request_id.clone(),
            session_id: self.binding.session_id.clone(),
            generation: self.binding.generation,
            kind: HarnessInteractionKind::Approval,
            prompt: format!("Approve native tool: {title}"),
            scope: tool_name
                .clone()
                .unwrap_or_else(|| "native_tool".to_owned()),
            choices,
            tool_name,
            input_preview,
        };
        if interaction.validate().is_err() {
            let _ = self
                .server
                .reply_result(&id, &json!({ "outcome": { "outcome": "cancelled" } }));
            return;
        }
        self.pending_permissions.insert(
            request_id,
            PendingPermission {
                rpc_id: id,
                interaction: interaction.clone(),
                allow_option_id,
                deny_option_id,
            },
        );
        self.push_observation(
            HarnessObservationKind::Interaction { interaction },
            Vec::new(),
        );
    }

    fn handle_async_reply(&mut self, id: u64, message: &Value) {
        let Some(turn) = self.active_turn.as_ref() else {
            return;
        };
        if turn.rpc_id != id {
            return;
        }
        let turn = self.active_turn.take().expect("checked above");
        self.withdraw_all_permissions();

        if let Some(flushed) = self.display.flush_active() {
            self.push_observation(flushed, Vec::new());
        }

        let frame_id = format!("acp-rpc-{id}");
        if let Some(err) = message.get("error") {
            let code = classify_acp_error(err);
            self.push_observation(
                HarnessObservationKind::Diagnostic {
                    code: code.to_owned(),
                    message: diagnostic_message(code).to_owned(),
                },
                vec![frame_id.clone()],
            );
            self.push_observation(
                HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::SubmissionTerminal {
                        attempt: turn.attempt,
                        native_turn_id: None,
                        native_frame_id: Some(frame_id.clone()),
                        disposition: if turn.interrupted {
                            HarnessTerminalDisposition::Interrupted
                        } else {
                            HarnessTerminalDisposition::Failed
                        },
                        output: None,
                    },
                },
                vec![frame_id],
            );
            return;
        }

        let result = message.get("result").unwrap_or(&Value::Null);
        if let Some(usage) = result.get("usage")
            && let Some(obs) = self.display.project_prompt_usage(usage)
        {
            self.push_observation(obs, vec![frame_id.clone()]);
        }

        let stop_reason = result
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn");
        let disposition = if turn.interrupted || stop_reason == "cancelled" {
            HarnessTerminalDisposition::Interrupted
        } else {
            HarnessTerminalDisposition::Completed
        };

        let output = self.display.last_finalized_text().map(str::to_owned);
        if let Some(text) = &output {
            self.push_observation(
                HarnessObservationKind::Result { text: text.clone() },
                vec![frame_id.clone()],
            );
        }

        self.push_observation(
            HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::SubmissionTerminal {
                    attempt: turn.attempt,
                    native_turn_id: None,
                    native_frame_id: Some(frame_id.clone()),
                    disposition,
                    output,
                },
            },
            vec![frame_id],
        );
    }

    fn withdraw_all_permissions(&mut self) {
        let keys: Vec<String> = self.pending_permissions.keys().cloned().collect();
        for request_id in keys {
            self.pending_permissions.remove(&request_id);
            self.push_observation(
                HarnessObservationKind::InteractionWithdrawn { request_id },
                Vec::new(),
            );
        }
    }

    fn stage_notice_once(&mut self, code: &'static str, message: &str) {
        if self.reported_notices.insert(code) {
            self.push_observation(
                HarnessObservationKind::Diagnostic {
                    code: code.to_owned(),
                    message: message.to_owned(),
                },
                Vec::new(),
            );
        }
    }

    fn push_observation(&mut self, kind: HarnessObservationKind, native_correlations: Vec<String>) {
        self.observation_seq = self.observation_seq.saturating_add(1);
        let observation = HarnessObservation {
            observation_id: format!(
                "acp-obs-{}-{}",
                self.binding.generation, self.observation_seq
            ),
            session_id: self.binding.session_id.clone(),
            generation: self.binding.generation,
            native_correlations,
            completeness: HarnessCompleteness::Complete,
            kind,
        };
        if observation.validate().is_ok() {
            self.staged.push_back(observation);
        }
    }
}

fn classify_acp_error(err: &Value) -> &'static str {
    if crate::auth::is_auth_required_error(Some(err)) {
        return "native_unauthorized";
    }
    let msg = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if msg.contains("rate") || msg.contains("quota") || msg.contains("429") {
        "native_rate_limited"
    } else if msg.contains("context") || msg.contains("token") {
        "native_context_window"
    } else {
        "native_turn_failed"
    }
}

const fn diagnostic_message(code: &str) -> &'static str {
    match code.as_bytes() {
        b"native_unauthorized" => "Antigravity authentication is required",
        b"native_rate_limited" => "Antigravity rate limit or quota reached",
        b"native_context_window" => "Antigravity context window exceeded",
        _ => "Antigravity native turn failed",
    }
}
