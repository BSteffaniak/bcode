//! Native app-server protocol interpretation. Host retains canonical mutation receipts.
use crate::transport::{AppServer, CodexError, Launch};
use bcode_harness_models::{
    HarnessActivity, HarnessAssistanceGrant, HarnessAttention, HarnessChoice, HarnessCompleteness,
    HarnessConnectivity, HarnessContinuation, HarnessDeliveryState, HarnessExecutionStatus,
    HarnessInteraction, HarnessInteractionKind, HarnessObservation, HarnessObservationKind,
    HarnessPlacement, HarnessReceipt, HarnessResponse, HarnessResponseValue, HarnessSessionBinding,
    HarnessSubmission, HarnessTerminalDisposition, MAX_HARNESS_ID_BYTES, MAX_HARNESS_OBSERVATIONS,
    MAX_HARNESS_TEXT_BYTES,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

struct Callback {
    native_id: Value,
    interaction: HarnessInteraction,
    method: String,
    question_ids: Vec<String>,
    remaining_questions: VecDeque<Value>,
    answers: BTreeMap<String, Value>,
    pending_question: bool,
    turn_id: String,
}

/// One admitted native thread owner. Create only after host permission and ownership checks.
pub struct CodexSession {
    server: AppServer,
    binding: HarnessSessionBinding,
    callbacks: BTreeMap<String, Callback>,
    sequence: u64,
    observation_epoch: uuid::Uuid,
    uncertain_submission: bool,
    active_turn: Option<String>,
    accepted_turn: Option<String>,
    last_submission: Option<(HarnessSubmission, HarnessReceipt)>,
    file_changes: BTreeMap<String, String>,
    observation_gap: bool,
    status: HarnessExecutionStatus,
    assistance_grant: Option<HarnessAssistanceGrant>,
    pending_terminal: Option<HarnessObservation>,
    pending_acceptance: Option<HarnessObservation>,
    last_terminal_turn: Option<String>,
    fresh_first_input: bool,
}

const fn unknown_status() -> HarnessExecutionStatus {
    HarnessExecutionStatus {
        connectivity: HarnessConnectivity::Unknown,
        activity: HarnessActivity::Unknown,
        attention: HarnessAttention::None,
        continuation: HarnessContinuation::Unknown,
        completeness: HarnessCompleteness::Partial,
    }
}

impl CodexSession {
    /// Initialize and start or resume a native thread in the exact admitted `CODEX_HOME`.
    /// Native thread creation is a mutation: an uncertain result must not be retried.
    ///
    /// # Errors
    /// Rejects malformed binding, unsupported protocol, or transport failure.
    pub fn open(
        launch: &Launch,
        binding: HarnessSessionBinding,
        resume: bool,
    ) -> Result<Self, CodexError> {
        Self::open_with_key(launch, binding, resume, None)
    }

    /// Open with an invocation-scoped vault key. Key material never enters observations.
    ///
    /// # Errors
    /// Fails closed unless effective native credential storage is ephemeral before key delivery.
    pub fn open_with_key(
        launch: &Launch,
        binding: HarnessSessionBinding,
        resume: bool,
        key: Option<&str>,
    ) -> Result<Self, CodexError> {
        Self::open_admitted(launch, binding, resume, key, None)
    }

    /// Open with an explicit host-admitted parent-assistance grant. No grant means no bridge.
    ///
    /// # Errors
    /// Rejects grants not correlated with this exact session and generation.
    pub fn open_admitted(
        launch: &Launch,
        binding: HarnessSessionBinding,
        resume: bool,
        key: Option<&str>,
        assistance_grant: Option<&HarnessAssistanceGrant>,
    ) -> Result<Self, CodexError> {
        Self::open_configured(launch, binding, resume, key, assistance_grant, false)
    }

    /// Open a fresh, ephemeral ChatGPT-subscription thread with restricted native execution.
    ///
    /// Uses the existing credential namespace without login or account mutation. The thread
    /// has a read-only sandbox, no environments, dynamic tools or selected capability roots,
    /// and no approval escalation. Shell, web, hooks, plugins and delegation are disabled.
    /// Native model-catalog utility tools are not a general-purpose tool allowlist.
    /// Callers must isolate process state (including `CODEX_SQLITE_HOME`) before launch.
    ///
    /// # Errors
    /// Rejects non-ChatGPT readiness, custom providers, configured MCP servers, unsupported
    /// native restrictions, malformed binding or transport failure. Never falls back to keys.
    pub fn open_ephemeral(
        launch: &Launch,
        binding: HarnessSessionBinding,
    ) -> Result<Self, CodexError> {
        if launch.ephemeral_credentials {
            return Err(CodexError::Configuration);
        }
        Self::open_configured(launch, binding, false, None, None, true)
    }

    fn open_configured(
        launch: &Launch,
        mut binding: HarnessSessionBinding,
        resume: bool,
        key: Option<&str>,
        assistance_grant: Option<&HarnessAssistanceGrant>,
        ephemeral: bool,
    ) -> Result<Self, CodexError> {
        if let Some(grant) = assistance_grant {
            grant.validate().map_err(|_| CodexError::Stale)?;
            if grant.child_session_id != binding.session_id
                || grant.child_generation != binding.generation
            {
                return Err(CodexError::Stale);
            }
        }
        // Verify bounded compatibility fields before any native side effects.
        binding.validate().map_err(|_| CodexError::Stale)?;
        if !resume && (key.is_some() != launch.ephemeral_credentials) {
            return Err(CodexError::Configuration);
        }
        let mut server = AppServer::spawn(launch)?;
        let observed_version = server.initialize(launch)?;
        if resume && binding.native_version != observed_version {
            return Err(CodexError::Unsupported);
        }
        binding.native_version = observed_version;
        Self::verify_delegation_disabled(&mut server, launch)?;
        Self::deliver_vault_key(&mut server, launch, key)?;
        // New thread has no live writer besides our private stdio. Remote control is a
        // separate native input route, so positively disable/verify it before creation.
        server.require_local_only()?;
        let result = if resume {
            server.call(
                "thread/resume",
                json!({"threadId":binding.native_session_id,"excludeTurns":true}),
            )?
        } else if ephemeral {
            Self::start_ephemeral(&mut server, launch)?
        } else {
            server.call("thread/start", json!({"cwd":launch.working_directory,"approvalPolicy":"untrusted","daybreakEnabled":false,
                "dynamicTools":if assistance_grant.is_some() { json!([{"type":"function","name":"bcode_parent_assistance",
                    "description":"Ask the owning Bcode session for assistance; not a tool execution permission",
                    "inputSchema":{"type":"object","properties":{"question":{"type":"string"}},"required":["question"],"additionalProperties":false}}]) } else { json!([]) }}))?
        };
        if !resume
            && (result["thread"]["status"]["type"].as_str() != Some("idle")
                || (!ephemeral && result["thread"]["daybreakEnabled"].as_bool() != Some(false))
                || !result["thread"]["turns"]
                    .as_array()
                    .is_some_and(Vec::is_empty))
        {
            server.fence();
            return Err(CodexError::Protocol);
        }
        Self::confirm_native_binding(&server, &mut binding, &result, resume)?;
        Ok(Self {
            server,
            binding,
            callbacks: BTreeMap::new(),
            sequence: 0,
            observation_epoch: uuid::Uuid::new_v4(),
            uncertain_submission: false,
            active_turn: None,
            accepted_turn: None,
            last_submission: None,
            file_changes: BTreeMap::new(),
            observation_gap: false,
            status: unknown_status(),
            assistance_grant: assistance_grant.cloned(),
            pending_terminal: None,
            pending_acceptance: None,
            last_terminal_turn: None,
            fresh_first_input: !resume,
        })
    }

    fn confirm_native_binding(
        server: &AppServer,
        binding: &mut HarnessSessionBinding,
        result: &Value,
        resume: bool,
    ) -> Result<(), CodexError> {
        let (gap, closed) = server.health()?;
        if gap || closed {
            return Err(CodexError::Uncertain);
        }
        let native_id = result["thread"]["id"]
            .as_str()
            .ok_or(CodexError::Protocol)?;
        if resume && native_id != binding.native_session_id {
            return Err(CodexError::Stale);
        }
        native_id.clone_into(&mut binding.native_session_id);
        if !resume {
            server.arm_first_input(&binding.native_session_id)?;
        }
        binding.validate().map_err(|_| CodexError::Protocol)
    }

    fn deliver_vault_key(
        server: &mut AppServer,
        launch: &Launch,
        key: Option<&str>,
    ) -> Result<(), CodexError> {
        if let Some(key) = key {
            if !launch.ephemeral_credentials || key.is_empty() || key.len() > 16 * 1024 {
                return Err(CodexError::Configuration);
            }
            let config = server.call(
                "config/read",
                json!({"includeLayers":false,"cwd":launch.working_directory}),
            )?;
            if config["config"]["cli_auth_credentials_store"].as_str() != Some("ephemeral")
                || config["config"]["model_provider"]
                    .as_str()
                    .or_else(|| config["config"]["modelProvider"].as_str())
                    != Some("openai")
                || !config["config"]["model_providers"]["openai"].is_null()
                || !config["config"]["modelProviders"]["openai"].is_null()
            {
                return Err(CodexError::Configuration);
            }
            server.call("account/login/start", json!({"type":"apiKey","apiKey":key}))?;
        }
        Ok(())
    }

    fn start_ephemeral(server: &mut AppServer, launch: &Launch) -> Result<Value, CodexError> {
        Self::verify_subscription(server, launch)?;
        let result = server.call(
            "thread/start",
            json!({
                "cwd":launch.working_directory,
                "modelProvider":"openai",
                "approvalPolicy":"never",
                "sandbox":"read-only",
                "ephemeral":true,
                "environments":[],
                "dynamicTools":[],
                "selectedCapabilityRoots":[],
                "config":{
                    "features.multi_agent":false,
                    "features.shell_tool":false,
                    "features.hooks":false,
                    "features.codex_hooks":false,
                    "features.plugin_hooks":false,
                    "features.plugins":false,
                    "features.enable_mcp_apps":false,
                    "features.skill_mcp_dependency_install":false,
                    "features.request_permissions_tool":false,
                    "features.tool_suggest":false,
                    "web_search":"disabled",
                    "tools.update_plan.enabled":false,
                    "tools.experimental_request_user_input.enabled":false,
                    "log_dir":launch.working_directory,
                    "sqlite_home":launch.working_directory
                }
            }),
        )?;
        if result["thread"]["ephemeral"].as_bool() != Some(true)
            || result["modelProvider"].as_str() != Some("openai")
            || result["approvalPolicy"].as_str() != Some("never")
            || result["sandbox"]["type"].as_str() != Some("readOnly")
        {
            server.fence();
            return Err(CodexError::Unsupported);
        }
        Ok(result)
    }

    fn verify_subscription(server: &mut AppServer, launch: &Launch) -> Result<(), CodexError> {
        let mut account = server.call("account/read", json!({"refreshToken":false}))?;
        let chatgpt = account["account"]["type"].as_str() == Some("chatgpt");
        crate::transport::wipe_value(&mut account);
        if !chatgpt {
            return Err(CodexError::Configuration);
        }
        let mut config = server.call(
            "config/read",
            json!({"includeLayers":false,"cwd":launch.working_directory}),
        )?;
        let provider = config["config"]["model_provider"]
            .as_str()
            .or_else(|| config["config"]["modelProvider"].as_str());
        let builtin = provider.is_none_or(|provider| provider == "openai")
            && config["config"]["model_providers"]["openai"].is_null()
            && config["config"]["modelProviders"]["openai"].is_null()
            && config["config"]["openai_base_url"].is_null()
            && config["config"]["chatgpt_base_url"]
                .as_str()
                .is_none_or(|url| url == "https://chatgpt.com/backend-api/");
        let no_mcp = ["mcp_servers", "mcpServers"].iter().all(|key| {
            let value = &config["config"][*key];
            value.is_null() || value.as_object().is_some_and(serde_json::Map::is_empty)
        });
        crate::transport::wipe_value(&mut config);
        if !builtin || !no_mcp {
            return Err(CodexError::Configuration);
        }
        Ok(())
    }

    fn verify_delegation_disabled(
        server: &mut AppServer,
        launch: &Launch,
    ) -> Result<(), CodexError> {
        let mut safety = server.call(
            "config/read",
            json!({"includeLayers":false,"cwd":launch.working_directory}),
        )?;
        let agents_disabled = safety["config"]["features"]["multi_agent"].as_bool() == Some(false);
        crate::transport::wipe_value(&mut safety);
        if !agents_disabled {
            return Err(CodexError::Unsupported);
        }
        Ok(())
    }

    /// Exact native association to persist through host admission.
    #[must_use]
    pub const fn binding(&self) -> &HarnessSessionBinding {
        &self.binding
    }

    /// Deliver once; an uncertain delivery fences subsequent submissions until host reconciliation.
    ///
    /// Host must admit each older identity exactly once; this adapter is not a durable dedup store.
    ///
    /// # Errors
    /// Rejects stale ownership, invalid submission or unsupported next mutation after uncertainty.
    pub fn submit(&mut self, submission: &HarnessSubmission) -> Result<HarnessReceipt, CodexError> {
        submission.validate().map_err(|_| CodexError::Bounds)?;
        self.binding
            .validate_owner(&submission.session_id, submission.generation)
            .map_err(|_| CodexError::Stale)?;
        if let Some((previous, receipt)) = &self.last_submission
            && (previous.submission_id == submission.submission_id
                || previous.attempt_id == submission.attempt_id)
        {
            return if previous == submission {
                Ok(receipt.clone())
            } else {
                Err(CodexError::Stale)
            };
        }
        if matches!(submission.placement, HarnessPlacement::NextTurn)
            && !self.fresh_first_input
            && !self.uncertain_submission
            && !self.observation_gap
        {
            return Err(CodexError::Unsupported);
        }
        if self.pending_acceptance.is_some() {
            // Never overwrite unobserved exact acceptance with another mutation.
            return Err(CodexError::Bounds);
        }
        // Canonical host admission deduplicates older identities. Keep only the last reply
        // for a lost immediate acknowledgment: no growing plugin journal or session lifetime cap.
        if self.uncertain_submission {
            return Err(CodexError::Uncertain);
        }
        if self.observation_gap {
            return Err(CodexError::Uncertain);
        }
        let (gap, closed) = self.server.health()?;
        if gap || closed {
            self.observation_gap = true;
            return Err(CodexError::Uncertain);
        }
        let input = json!([{"type":"text","text":submission.text,"text_elements":[]}]);
        let (method, params) = match &submission.placement {
            // This is a single-use construction proof, not a racy idle snapshot:
            // fresh native thread, private stdio, remote control fenced, delegation disabled.
            HarnessPlacement::NextTurn if self.fresh_first_input => {
                self.fresh_first_input = false;
                self.server.consume_first_input()?;
                (
                    "turn/start",
                    json!({"threadId":self.binding.native_session_id,"input":input}),
                )
            }
            HarnessPlacement::NextTurn => return Err(CodexError::Unsupported),
            HarnessPlacement::Steer {
                expected_native_turn_id,
            } => {
                // Any input attempt forfeits the fresh construction proof, even rejection.
                self.fresh_first_input = false;
                self.server.consume_first_input()?;
                (
                    "turn/steer",
                    json!({"threadId":self.binding.native_session_id,
                    "expectedTurnId":expected_native_turn_id,"input":input}),
                )
            }
        };
        // Fence before delivery, not after acknowledgment. Host must retain its own attempt first.
        self.uncertain_submission = true;
        let request_id = self.server.next_request_id();
        let result = self.server.call(method, params);
        let receipt = self.record_submission_ack(submission, method, request_id, &result)?;
        self.last_submission = Some((submission.clone(), receipt.clone()));
        Ok(receipt)
    }

    fn record_submission_ack(
        &mut self,
        submission: &HarnessSubmission,
        method: &str,
        request_id: String,
        result: &Result<Value, CodexError>,
    ) -> Result<HarnessReceipt, CodexError> {
        let (state, native_turn_id) = match result {
            Ok(value) => {
                self.uncertain_submission = false;
                let turn = match method {
                    "turn/start" => value["turn"]["id"].as_str(),
                    "turn/steer" => value["turnId"].as_str(),
                    _ => None,
                };
                if turn.is_some_and(|turn| !turn.is_empty() && turn.len() <= MAX_HARNESS_ID_BYTES) {
                    (
                        HarnessDeliveryState::NativeAccepted,
                        turn.map(str::to_owned),
                    )
                } else {
                    self.uncertain_submission = true;
                    self.server.fence();
                    (HarnessDeliveryState::Uncertain, None)
                }
            }
            Err(CodexError::Rejected) => {
                self.uncertain_submission = false;
                (HarnessDeliveryState::Rejected, None)
            }
            Err(_) => (HarnessDeliveryState::Uncertain, None),
        };
        if let Some(turn_id) = native_turn_id {
            let acceptance = HarnessObservation {
                observation_id: format!(
                    "codex-accepted:{}:{}:{}",
                    self.binding.generation, self.observation_epoch, request_id
                ),
                session_id: self.binding.session_id.clone(),
                generation: self.binding.generation,
                native_correlations: vec![turn_id.clone()],
                completeness: HarnessCompleteness::Complete,
                kind: HarnessObservationKind::SubmissionAccepted {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                    native_turn_id: turn_id.clone(),
                },
            };
            if acceptance.validate().is_err() {
                self.uncertain_submission = true;
                self.server.fence();
                return Err(CodexError::Protocol);
            }
            self.accepted_turn = Some(turn_id.clone());
            self.active_turn = Some(turn_id);
            self.pending_acceptance = Some(acceptance);
        }
        Ok(HarnessReceipt {
            submission_id: submission.submission_id.clone(),
            attempt_id: submission.attempt_id.clone(),
            session_id: submission.session_id.clone(),
            generation: submission.generation,
            state,
            native_request_id: Some(request_id),
        })
    }

    /// Request interruption of the retained named native turn; acknowledgment is not settlement.
    /// Native checks precede a generic interrupt operation, so this is not an atomic
    /// expected-turn fence across a concurrent native turn transition.
    ///
    /// # Errors
    /// Rejects stale generations, empty turns, or native/transport failure.
    pub fn interrupt(&mut self, generation: u64, turn_id: &str) -> Result<(), CodexError> {
        self.binding
            .validate_owner(&self.binding.session_id, generation)
            .map_err(|_| CodexError::Stale)?;
        if turn_id.is_empty() || turn_id.len() > MAX_HARNESS_ID_BYTES {
            return Err(CodexError::Bounds);
        }
        if self.active_turn.as_deref() != Some(turn_id) {
            return Err(CodexError::Stale);
        }
        let (gap, closed) = self.server.health()?;
        if self.observation_gap || gap || closed {
            return Err(CodexError::Uncertain);
        }
        self.server.call(
            "turn/interrupt",
            json!({"threadId":self.binding.native_session_id,"turnId":turn_id}),
        )?;
        Ok(())
    }

    /// Interrupt only the currently observed native turn; never guess a turn after reconnection.
    ///
    /// # Errors
    /// Returns uncertain if no exact active turn is retained.
    pub fn interrupt_active(&mut self, generation: u64) -> Result<(), CodexError> {
        let turn = self.active_turn.clone().ok_or(CodexError::Uncertain)?;
        self.interrupt(generation, &turn)
    }

    /// Metadata-only reconciliation. Never hydrate full native history or resend an uncertain turn.
    ///
    /// # Errors
    /// Returns protocol/transport errors; absence of a live turn never proves no prior effects.
    pub fn reconcile(&mut self) -> Result<HarnessExecutionStatus, CodexError> {
        let value = self.server.call(
            "thread/read",
            json!({"threadId":self.binding.native_session_id,"includeTurns":false}),
        )?;
        if value["thread"]["id"].as_str() != Some(self.binding.native_session_id.as_str()) {
            return Err(CodexError::Stale);
        }
        self.status.connectivity = HarnessConnectivity::Connected;
        self.status.activity = match value["thread"]["status"]["type"].as_str() {
            Some("active") => HarnessActivity::Running,
            Some("idle") => HarnessActivity::Idle,
            _ => HarnessActivity::Unknown,
        };
        self.status.completeness = HarnessCompleteness::Partial;
        Ok(self.status.clone())
    }

    /// Bounded native turn evidence. Terminal outcome does not prove background quiescence.
    ///
    /// # Errors
    /// Returns malformed protocol, callback overflow, or invalid normalized observations.
    pub fn observe(
        &mut self,
        limit: usize,
    ) -> Result<(Vec<HarnessObservation>, HarnessCompleteness), CodexError> {
        if limit == 0 || limit > MAX_HARNESS_OBSERVATIONS {
            return Err(CodexError::Bounds);
        }
        if let Some(acceptance) = self.pending_acceptance.take() {
            // An explicit acknowledged association remains evidence even if transport was
            // subsequently lost. Deliver it before any started/completed/settlement event.
            return Ok((vec![acceptance], HarnessCompleteness::Partial));
        }
        if self.pending_terminal.is_some() {
            let (gap, closed) = self.server.health()?;
            if gap || closed {
                self.pending_terminal = None;
                self.observation_gap = true;
            } else if let Some(terminal) = self.pending_terminal.take() {
                return Ok((vec![terminal], HarnessCompleteness::Partial));
            }
        }
        let (events, gap, closed) = self.server.events((limit / 2).max(1))?;
        if gap || closed {
            self.observation_gap = true;
            self.callbacks.clear();
            self.status = unknown_status();
            self.status.connectivity = HarnessConnectivity::Disconnected;
            self.status.completeness = HarnessCompleteness::Gap;
            return Ok((Vec::new(), HarnessCompleteness::Gap));
        }
        let mut observations = Vec::new();
        for event in events {
            let terminal_turn = if !self.observation_gap
                && !self.uncertain_submission
                && event["method"] == "turn/completed"
                && event["params"]["threadId"].as_str()
                    == Some(self.binding.native_session_id.as_str())
                && self.active_turn.is_some()
                && event["params"]["turn"]["id"].as_str() == self.active_turn.as_deref()
                && event["params"]["turn"]["id"].as_str() == self.accepted_turn.as_deref()
            {
                event["params"]["turn"]["id"].as_str().map(str::to_owned)
            } else {
                None
            };
            match self.normalize(&event) {
                Ok(Some(observation)) => observations.push(observation),
                Ok(None) => {}
                Err(error) => {
                    self.observation_gap = true;
                    self.callbacks.clear();
                    self.server.fence();
                    return Err(error);
                }
            }
            if let Some(turn_id) = terminal_turn {
                let terminal = self.terminal_observation(&event, turn_id)?;
                if observations.len() < limit {
                    observations.push(terminal);
                } else {
                    self.pending_terminal = Some(terminal);
                }
                // Native task completion records live background-terminal counts;
                // it does not close those processes. Neither completed, interrupted
                // nor failed proves quiescence. Do not synthesize Settlement here.
            }
        }
        if observations.is_empty()
            && !self.observation_gap
            && let Some(callback) = self
                .callbacks
                .values_mut()
                .find(|callback| callback.pending_question)
        {
            callback.pending_question = false;
            self.sequence = self.sequence.checked_add(1).ok_or(CodexError::Bounds)?;
            let observation = HarnessObservation {
                observation_id: format!(
                    "codex:{}:{}:{}",
                    self.binding.generation, self.observation_epoch, self.sequence
                ),
                session_id: self.binding.session_id.clone(),
                generation: self.binding.generation,
                native_correlations: vec![callback.turn_id.clone()],
                completeness: HarnessCompleteness::Complete,
                kind: HarnessObservationKind::Interaction {
                    interaction: callback.interaction.clone(),
                },
            };
            observation.validate().map_err(|_| CodexError::Protocol)?;
            observations.push(observation);
        }
        let completeness = if gap || self.observation_gap {
            HarnessCompleteness::Gap
        } else if closed {
            HarnessCompleteness::Unknown
        } else {
            HarnessCompleteness::Partial
        };
        self.status.completeness = completeness;
        Ok((observations, completeness))
    }

    fn terminal_observation(
        &mut self,
        event: &Value,
        turn_id: String,
    ) -> Result<HarnessObservation, CodexError> {
        let disposition = match event["params"]["turn"]["status"].as_str() {
            Some("completed") if event["params"]["turn"]["error"].is_null() => {
                HarnessTerminalDisposition::Completed
            }
            Some("interrupted") => HarnessTerminalDisposition::Interrupted,
            Some("failed") => HarnessTerminalDisposition::Failed,
            _ => return Err(CodexError::Protocol),
        };
        self.sequence = self.sequence.checked_add(1).ok_or(CodexError::Bounds)?;
        let terminal = HarnessObservation {
            observation_id: format!(
                "codex:{}:{}:{}",
                self.binding.generation, self.observation_epoch, self.sequence
            ),
            session_id: self.binding.session_id.clone(),
            generation: self.binding.generation,
            native_correlations: vec![turn_id.clone()],
            completeness: HarnessCompleteness::Complete,
            kind: HarnessObservationKind::NativeTurnTerminal {
                native_turn_id: turn_id,
                disposition,
            },
        };
        terminal.validate().map_err(|_| CodexError::Protocol)?;
        Ok(terminal)
    }

    /// Last normalized native execution evidence; not proof of assignment completion.
    #[must_use]
    pub fn status(&self) -> HarnessExecutionStatus {
        self.status.clone()
    }

    fn normalize(&mut self, event: &Value) -> Result<Option<HarnessObservation>, CodexError> {
        let method = event["method"].as_str().ok_or(CodexError::Protocol)?;
        let params = &event["params"];
        // Cross-thread notifications cannot attach evidence or approval authority to this owner.
        if event.get("id").is_some()
            && params["threadId"].as_str() != Some(self.binding.native_session_id.as_str())
        {
            self.server.send(json!({"id":event["id"],"error":{"code":-32600,"message":"Callback does not match owned thread"}}))?;
            return Ok(None);
        }
        if params
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|id| id != self.binding.native_session_id)
        {
            return Ok(None);
        }
        if method == "serverRequest/resolved" {
            if params["threadId"].as_str() != Some(self.binding.native_session_id.as_str()) {
                return Ok(None);
            }
            let native_id = params.get("requestId").ok_or(CodexError::Protocol)?;
            if !native_id.is_string() && !native_id.is_i64() {
                return Err(CodexError::Protocol);
            }
            self.callbacks
                .retain(|_, callback| callback.native_id != *native_id);
            if self.callbacks.is_empty() {
                self.status.attention = HarnessAttention::None;
            }
            return Ok(None);
        }
        if matches!(method, "turn/started" | "turn/completed")
            && params["threadId"].as_str() != Some(self.binding.native_session_id.as_str())
        {
            return Ok(None);
        }
        self.retain_file_change_diff(method, params)?;
        if method == "thread/started" {
            if params["thread"]["id"].as_str() != Some(self.binding.native_session_id.as_str()) {
                return Ok(None);
            }
            self.status.connectivity = HarnessConnectivity::Connected;
        }
        if !self.update_native_turn(method, params)? {
            return Ok(None);
        }
        if !params["threadId"].is_string() && method != "thread/started" {
            return Ok(None);
        }
        self.sequence = self.sequence.checked_add(1).ok_or(CodexError::Bounds)?;
        let observation_id = format!(
            "codex:{}:{}:{}",
            self.binding.generation, self.observation_epoch, self.sequence
        );
        let kind = if let Some(native_id) = event.get("id") {
            let Some(kind) = self.normalize_callback(method, params, native_id, &observation_id)?
            else {
                return Ok(None);
            };
            kind
        } else {
            let Some(kind) = self.normalize_notification(method, params)? else {
                return Ok(None);
            };
            kind
        };
        let mut native_correlations = Vec::new();
        for value in [
            params["turnId"]
                .as_str()
                .or_else(|| params["turn"]["id"].as_str()),
            params["itemId"]
                .as_str()
                .or_else(|| params["item"]["id"].as_str()),
            params["callId"].as_str(),
        ]
        .into_iter()
        .flatten()
        {
            native_correlations.push(value.to_owned());
        }
        let completeness = if matches!(kind, HarnessObservationKind::Result { .. }) {
            HarnessCompleteness::Complete
        } else {
            HarnessCompleteness::Partial
        };
        let observation = HarnessObservation {
            observation_id,
            session_id: self.binding.session_id.clone(),
            generation: self.binding.generation,
            native_correlations,
            kind,
            completeness,
        };
        observation.validate().map_err(|_| CodexError::Bounds)?;
        Ok(Some(observation))
    }

    // False skips stale turn evidence; errors retain the same pre-normalization fence.
    fn update_native_turn(&mut self, method: &str, params: &Value) -> Result<bool, CodexError> {
        if method == "turn/started" {
            if self.fresh_first_input {
                self.fresh_first_input = false;
                self.uncertain_submission = true;
                self.server.fence();
                return Err(CodexError::Uncertain);
            }
            let turn_id = params["turn"]["id"].as_str().ok_or(CodexError::Protocol)?;
            if self.last_terminal_turn.as_deref() == Some(turn_id) {
                return Ok(false);
            }
            if turn_id.is_empty() || turn_id.len() > MAX_HARNESS_ID_BYTES {
                return Err(CodexError::Protocol);
            }
            self.active_turn = Some(turn_id.to_owned());
        } else if method == "turn/completed" {
            if params["turn"]["id"].as_str() != self.active_turn.as_deref() {
                return Ok(false);
            }
            self.last_terminal_turn = self.active_turn.take();
            if let Some(turn_id) = params["turn"]["id"].as_str() {
                self.callbacks
                    .retain(|_, callback| callback.turn_id != turn_id);
                self.file_changes.clear();
            }
        }
        Ok(true)
    }

    /// Retain only bounded native diffs for later exact-item approval; absence never grants approval.
    fn retain_file_change_diff(&mut self, method: &str, params: &Value) -> Result<(), CodexError> {
        if method == "item/started"
            && params["item"]["type"].as_str() == Some("fileChange")
            && let Some(item_id) = params["item"]["id"].as_str()
        {
            let changes = serde_json::to_string(&params["item"]["changes"])
                .map_err(|_| CodexError::Protocol)?;
            if item_id.len() <= MAX_HARNESS_ID_BYTES
                && params["item"]["changes"].is_array()
                && changes.len() <= MAX_HARNESS_TEXT_BYTES / 2
                && self.file_changes.len() < 8
            {
                self.file_changes.insert(item_id.to_owned(), changes);
            }
        }
        Ok(())
    }

    fn normalize_callback(
        &mut self,
        method: &str,
        params: &Value,
        native_id: &Value,
        observation_id: &str,
    ) -> Result<Option<HarnessObservationKind>, CodexError> {
        if self.observation_gap {
            self.server.send(json!({"id":native_id,"error":{"code":-32603,"message":"Observation gap requires reconciliation"}}))?;
            return Ok(None);
        }
        if self.callbacks.len() >= 32 {
            return Err(CodexError::Bounds);
        }
        if params["turnId"].as_str() != self.active_turn.as_deref() || self.active_turn.is_none() {
            self.server.send(json!({"id":native_id,"error":{"code":-32600,"message":"Callback does not match active turn"}}))?;
            return Ok(None);
        }
        let request_id = observation_id.to_owned();
        let Some((kind, prompt)) = self.callback_admission(method, params, native_id)? else {
            return Ok(None);
        };
        let mut interaction_params = params.clone();
        let mut remaining_questions = VecDeque::new();
        if kind == HarnessInteractionKind::Question {
            let questions = params["questions"].as_array().ok_or(CodexError::Protocol)?;
            if questions.is_empty() || questions.len() > 32 {
                return Err(CodexError::Bounds);
            }
            let mut ids = BTreeSet::new();
            for question in questions {
                let id = question["id"].as_str().ok_or(CodexError::Protocol)?;
                if id.is_empty()
                    || id.len() > MAX_HARNESS_ID_BYTES
                    || id.trim() != id
                    || id.chars().any(char::is_control)
                    || !ids.insert(id)
                {
                    return Err(CodexError::Protocol);
                }
                // Validate every question before exposing any part of the native batch.
                let single = json!({"questions":[question]});
                self.callback_interaction(method, &single, &request_id, kind, prompt)?;
            }
            remaining_questions.extend(questions.iter().skip(1).cloned());
            interaction_params["questions"] = json!([questions[0]]);
        }
        let (interaction, question_ids) =
            self.callback_interaction(method, &interaction_params, &request_id, kind, prompt)?;
        self.callbacks.insert(
            request_id,
            Callback {
                native_id: native_id.clone(),
                interaction: interaction.clone(),
                method: method.to_owned(),
                question_ids,
                remaining_questions,
                answers: BTreeMap::new(),
                pending_question: false,
                turn_id: params["turnId"]
                    .as_str()
                    .ok_or(CodexError::Protocol)?
                    .to_owned(),
            },
        );
        self.status.attention = match kind {
            HarnessInteractionKind::Approval => HarnessAttention::Approval,
            HarnessInteractionKind::Question => HarnessAttention::Question,
            HarnessInteractionKind::Assistance => HarnessAttention::Assistance,
        };
        Ok(Some(HarnessObservationKind::Interaction { interaction }))
    }

    fn callback_admission(
        &self,
        method: &str,
        params: &Value,
        native_id: &Value,
    ) -> Result<Option<(HarnessInteractionKind, &'static str)>, CodexError> {
        let (kind, prompt) = match method {
            "item/commandExecution/requestApproval" => (
                HarnessInteractionKind::Approval,
                "Codex requests approval for a native operation",
            ),
            "item/fileChange/requestApproval" => {
                let known_diff = params["itemId"]
                    .as_str()
                    .is_some_and(|id| self.file_changes.contains_key(id));
                if !params["grantRoot"].is_null() || !known_diff {
                    // Never reinterpret a root grant or approve a missing/evicted diff blindly.
                    self.server
                        .send(json!({"id":native_id,"result":{"decision":"decline"}}))?;
                    return Ok(None);
                }
                (
                    HarnessInteractionKind::Approval,
                    "Codex requests approval for these native file changes",
                )
            }
            "item/tool/requestUserInput" => (
                HarnessInteractionKind::Question,
                "Codex requests user input",
            ),
            "item/tool/call"
                if self.assistance_grant.is_some()
                    && params["tool"].as_str() == Some("bcode_parent_assistance")
                    && params.get("namespace").is_none_or(Value::is_null) =>
            {
                (
                    HarnessInteractionKind::Assistance,
                    "Codex requests parent assistance",
                )
            }
            "item/tool/call" => {
                // Native tools are never routed to Bcode tool dispatch, even if names match.
                self.server.send(json!({"id":native_id,"result":{"contentItems":[{"type":"inputText","text":"Dynamic tool not registered by Bcode"}],"success":false}}))?;
                return Ok(None);
            }
            _ => {
                self.server.send(json!({"id":native_id,"error":{"code":-32601,"message":"Unsupported callback"}}))?;
                return Ok(None);
            }
        };
        if params["questions"]
            .as_array()
            .is_some_and(|qs| qs.iter().any(|q| q["isSecret"].as_bool() == Some(true)))
        {
            // Generic semantic questions are not a secret-input transport.
            self.server
                .send(json!({"id":native_id,"result":{"answers":{}}}))?;
            return Ok(None);
        }
        if method == "item/commandExecution/requestApproval"
            && (params["additionalPermissions"].is_object()
                || params["networkApprovalContext"].is_object()
                || params["command"].as_str().is_none()
                || params["availableDecisions"]
                    .as_array()
                    .is_some_and(|decisions| {
                        !decisions.iter().any(|decision| decision == "accept")
                    }))
        {
            self.server
                .send(json!({"id":native_id,"result":{"decision":"decline"}}))?;
            return Ok(None);
        }
        Ok(Some((kind, prompt)))
    }

    fn callback_interaction(
        &self,
        method: &str,
        params: &Value,
        request_id: &str,
        kind: HarnessInteractionKind,
        prompt: &str,
    ) -> Result<(HarnessInteraction, Vec<String>), CodexError> {
        let question_ids = params["questions"]
            .as_array()
            .map(|questions| {
                questions
                    .iter()
                    .filter_map(|q| q["id"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if question_ids.len() > 32 {
            return Err(CodexError::Bounds);
        }
        let question_text = params["questions"].as_array().map(|qs| {
            qs.iter()
                .filter_map(|q| q["question"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        });
        let interaction = HarnessInteraction {
            request_id: request_id.to_owned(),
            session_id: self.binding.session_id.clone(),
            generation: self.binding.generation,
            kind,
            prompt: if kind == HarnessInteractionKind::Assistance {
                params["arguments"]["question"]
                    .as_str()
                    .ok_or(CodexError::Protocol)?
                    .to_owned()
            } else {
                question_text.unwrap_or_else(|| prompt.to_owned())
            },
            scope: if method == "item/fileChange/requestApproval" {
                self.file_changes
                    .get(params["itemId"].as_str().ok_or(CodexError::Protocol)?)
                    .cloned()
                    .ok_or(CodexError::Unsupported)?
            } else if kind == HarnessInteractionKind::Approval {
                // Include native operation identity/diff rather than grant blind approval.
                serde_json::to_string(&json!({"command":params["command"],"cwd":params["cwd"],
                        "reason":params["reason"],"changes":params["changes"],"itemId":params["itemId"]}))
                        .map_err(|_| CodexError::Protocol)?
            } else {
                "Native Codex callback; no Bcode tool dispatch".to_owned()
            },
            choices: if question_ids.len() == 1 {
                params["questions"][0]["options"]
                    .as_array()
                    .map(|options| {
                        options
                            .iter()
                            .enumerate()
                            .filter_map(|(index, option)| {
                                option["label"].as_str().map(|label| HarnessChoice {
                                    choice_id: format!("option-{index}"),
                                    label: label.to_owned(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
        };
        interaction.validate().map_err(|_| CodexError::Bounds)?;
        Ok((interaction, question_ids))
    }

    fn completed_agent_message(
        &self,
        params: &Value,
    ) -> Result<Option<HarnessObservationKind>, CodexError> {
        let turn_id = params["turnId"].as_str();
        if self.observation_gap
            || self.uncertain_submission
            || self.active_turn.is_none()
            || turn_id != self.active_turn.as_deref()
            || turn_id != self.accepted_turn.as_deref()
        {
            // A completed item cannot adopt an unaccepted turn or revive a terminal
            // turn. Wrong/missing correlation never supplies workflow output.
            return Ok(None);
        }
        let item = &params["item"];
        let item_id = item["id"].as_str().ok_or(CodexError::Protocol)?;
        if item_id.is_empty()
            || item_id.len() > MAX_HARNESS_ID_BYTES
            || item_id.trim() != item_id
            || item_id.chars().any(char::is_control)
        {
            return Err(CodexError::Protocol);
        }
        let text = item["text"].as_str().ok_or(CodexError::Protocol)?;
        if text.len() > MAX_HARNESS_TEXT_BYTES {
            // Never truncate an authoritative result into a different answer.
            return Err(CodexError::Bounds);
        }
        if item["phase"].as_str() == Some("final_answer") {
            Ok(Some(HarnessObservationKind::Result {
                text: text.to_owned(),
            }))
        } else {
            // Item completion is not turn completion. Commentary, partial answers
            // and legacy/unknown phases remain display content, never final output.
            Ok(Some(HarnessObservationKind::Text {
                text: text.to_owned(),
            }))
        }
    }

    fn normalize_notification(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Result<Option<HarnessObservationKind>, CodexError> {
        let kind = match method {
            "item/agentMessage/delta" => HarnessObservationKind::Text {
                text: params["delta"]
                    .as_str()
                    .ok_or(CodexError::Protocol)?
                    .to_owned(),
            },
            "item/started" | "item/completed" => {
                let item = &params["item"];
                match item["type"].as_str() {
                    Some("agentMessage") if method == "item/completed" => {
                        return self.completed_agent_message(params);
                    }
                    Some("commandExecution" | "fileChange" | "mcpToolCall" | "dynamicToolCall") => {
                        HarnessObservationKind::Tool {
                            native_tool_id: item["id"]
                                .as_str()
                                .ok_or(CodexError::Protocol)?
                                .to_owned(),
                            name: item["type"]
                                .as_str()
                                .ok_or(CodexError::Protocol)?
                                .to_owned(),
                            status: method.to_owned(),
                        }
                    }
                    Some("collabAgentToolCall") => {
                        self.observation_gap = true;
                        self.status.continuation = HarnessContinuation::Unknown;
                        HarnessObservationKind::Subagent {
                            native_agent_id: item["id"]
                                .as_str()
                                .ok_or(CodexError::Protocol)?
                                .to_owned(),
                            status: method.to_owned(),
                        }
                    }
                    _ => return Ok(None),
                }
            }
            "turn/started" => {
                if params["turn"]["status"].as_str() != Some("inProgress") {
                    return Err(CodexError::Protocol);
                }
                self.status.connectivity = HarnessConnectivity::Connected;
                self.status.activity = HarnessActivity::Running;
                self.status.attention = HarnessAttention::None;
                HarnessObservationKind::Status {
                    status: self.status.clone(),
                }
            }
            "turn/completed" => self.completed_turn_kind(params)?,
            "thread/started" | "thread/status/changed" => {
                let status = if method == "thread/started" {
                    &params["thread"]["status"]
                } else {
                    &params["status"]
                };
                self.status.connectivity = HarnessConnectivity::Connected;
                self.status.activity = match status["type"].as_str() {
                    Some("active") => HarnessActivity::Running,
                    Some("idle") => HarnessActivity::Idle,
                    _ => HarnessActivity::Unknown,
                };
                HarnessObservationKind::Status {
                    status: self.status.clone(),
                }
            }
            // Ignore unsupported vendor payloads rather than persist unsanitized protocol dumps.
            _ => return Ok(None),
        };
        Ok(Some(kind))
    }

    fn completed_turn_kind(
        &mut self,
        params: &Value,
    ) -> Result<HarnessObservationKind, CodexError> {
        if params["turn"]["status"] == "completed" && !params["turn"]["error"].is_null() {
            return Err(CodexError::Protocol);
        }
        let kind = {
            self.status.connectivity = HarnessConnectivity::Connected;
            self.status.attention = HarnessAttention::None;
            self.status.continuation = HarnessContinuation::Unknown;
            self.status.activity = match params["turn"]["status"].as_str() {
                Some("completed" | "failed") => HarnessActivity::Unknown,
                Some("interrupted") => HarnessActivity::Interrupted,
                _ => return Err(CodexError::Protocol),
            };
            if params["turn"]["status"] == "failed" {
                // Native error.message may contain credentials; retain category only.
                HarnessObservationKind::Diagnostic {
                    code: "native_turn_failed".to_owned(),
                    message: "Codex native turn failed; native error details withheld".to_owned(),
                }
            } else {
                HarnessObservationKind::Status {
                    status: self.status.clone(),
                }
            }
        };
        Ok(kind)
    }

    fn answer_question(&mut self, request_id: &str, answer: &str) -> Result<(), CodexError> {
        let mut callback = self.callbacks.remove(request_id).ok_or(CodexError::Stale)?;
        callback.answers.insert(
            callback.question_ids[0].clone(),
            json!({"answers":[answer]}),
        );
        if let Some(question) = callback.remaining_questions.pop_front() {
            let request_id = format!("codex-question:{}", uuid::Uuid::new_v4());
            let (interaction, question_ids) = self.callback_interaction(
                &callback.method,
                &json!({"questions":[question]}),
                &request_id,
                HarnessInteractionKind::Question,
                "Codex requests user input",
            )?;
            callback.interaction = interaction;
            callback.question_ids = question_ids;
            callback.pending_question = true;
            self.callbacks.insert(request_id, callback);
            return Ok(());
        }
        if self.callbacks.is_empty() {
            self.status.attention = HarnessAttention::None;
        }
        self.server
            .send(json!({"id":callback.native_id,"result":{"answers":callback.answers}}))
    }

    /// Respond to one exact generation-fenced native callback. Responses are consumed before send.
    ///
    /// # Errors
    /// Returns stale, mismatched, unsupported or uncertain delivery; no ambiguous callback replay.
    pub fn respond(&mut self, response: &HarnessResponse) -> Result<(), CodexError> {
        let (gap, closed) = self.server.health()?;
        if self.observation_gap || gap || closed {
            return Err(CodexError::Uncertain);
        }
        let callback = self
            .callbacks
            .get(&response.request_id)
            .ok_or(CodexError::Stale)?;
        response
            .validate_for(&callback.interaction)
            .map_err(|_| CodexError::Stale)?;
        if self.active_turn.as_deref() != Some(callback.turn_id.as_str())
            || callback.pending_question
        {
            return Err(CodexError::Stale);
        }
        let result = match &response.value {
            HarnessResponseValue::Approval { approved } => {
                json!({"decision":if *approved {"accept"} else {"decline"}})
            }
            HarnessResponseValue::Cancel if callback.method == "item/tool/call" => {
                json!({"contentItems":[],"success":false})
            }
            HarnessResponseValue::Cancel if callback.method == "item/tool/requestUserInput" => {
                json!({"answers":{}})
            }
            HarnessResponseValue::Cancel => json!({"decision":"cancel"}),
            HarnessResponseValue::Answer { choice_id, text } => {
                if callback.question_ids.len() != 1 {
                    return Err(CodexError::Unsupported);
                }
                let answer = if let Some(choice_id) = choice_id {
                    callback
                        .interaction
                        .choices
                        .iter()
                        .find(|choice| choice.choice_id == *choice_id)
                        .map(|choice| choice.label.as_str())
                        .ok_or(CodexError::Stale)?
                } else {
                    text.as_str()
                };
                let answer = answer.to_owned();
                return self.answer_question(&response.request_id, &answer);
            }
            HarnessResponseValue::Assistance { text } => {
                json!({"contentItems":[{"type":"inputText","text":text}],"success":true})
            }
        };
        let callback = self
            .callbacks
            .remove(&response.request_id)
            .ok_or(CodexError::Stale)?;
        if self.callbacks.is_empty() {
            self.status.attention = HarnessAttention::None;
        }
        self.server
            .send(json!({"id":callback.native_id,"result":result}))
    }
}
