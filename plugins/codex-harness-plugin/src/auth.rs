//! Native authentication on a dedicated app-server, in the same `CODEX_HOME` namespace.
use crate::transport::{AppServer, CodexError, Launch};
use bcode_harness_models::{HarnessAuthBinding, HarnessAuthStep};
use bcode_provider_auth_models::managed::{
    AuthCustodyMode, AuthNamespace, AuthReadiness, ManagedAuthFailure, ManagedAuthStatus,
    SensitiveAuthInteraction,
};
use serde_json::json;
use std::collections::BTreeMap;

struct AuthFlow {
    login_id: String,
    completion: Option<bool>,
    terminal: Option<ManagedAuthStatus>,
}

/// Ephemeral native login coordinator, never persisted or replayed after daemon restart.
pub struct CodexAuth {
    server: AppServer,
    binding: HarnessAuthBinding,
    flows: BTreeMap<String, AuthFlow>,
    ephemeral_verified: bool,
}
impl CodexAuth {
    /// Start native auth inspection in an explicitly admitted namespace.
    ///
    /// # Errors
    /// Rejects namespace mismatch, incompatible config or transport errors.
    pub fn open(launch: &Launch, binding: HarnessAuthBinding) -> Result<Self, CodexError> {
        binding.validate().map_err(|_| CodexError::Configuration)?;
        if binding.credential_namespace != launch.codex_home.to_string_lossy()
            || (binding.custody == AuthCustodyMode::VaultManaged && !launch.ephemeral_credentials)
        {
            return Err(CodexError::Configuration);
        }
        let mut server = AppServer::spawn(launch)?;
        server.initialize(launch)?;
        let ephemeral_verified = if launch.ephemeral_credentials {
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
            true
        } else {
            false
        };
        Ok(Self {
            server,
            binding,
            flows: BTreeMap::new(),
            ephemeral_verified,
        })
    }

    /// Inspect local readiness; refresh is not remote verification.
    ///
    /// # Errors
    /// Returns normalized transport or protocol failures without native account details.
    pub fn status(&mut self) -> Result<ManagedAuthStatus, CodexError> {
        let mut result = self
            .server
            .call("account/read", json!({"refreshToken":false}))?;
        let ready = result
            .get("account")
            .ok_or(CodexError::Protocol)?
            .is_object();
        crate::transport::wipe_value(&mut result);
        Ok(self.status_value(if ready {
            AuthReadiness::Ready
        } else {
            AuthReadiness::LoginRequired
        }))
    }

    fn status_value(&self, readiness: AuthReadiness) -> ManagedAuthStatus {
        ManagedAuthStatus {
            namespace: AuthNamespace {
                plugin_id: "bcode.codex-harness".to_owned(),
                credential_namespace: self.binding.credential_namespace.clone(),
            },
            custody: self.binding.custody,
            readiness,
        }
    }

    /// Begin supported native browser/device login. Sensitive URL/code is returned only as an auth effect.
    ///
    /// # Errors
    /// Rejects wrong custody, unsupported methods, duplicate flows or native failure.
    pub fn begin(&mut self, flow_id: &str, method: &str) -> Result<HarnessAuthStep, CodexError> {
        if self.binding.custody != AuthCustodyMode::NativeManaged {
            return Err(CodexError::Unsupported);
        }
        if self.flows.contains_key(flow_id) {
            return Err(CodexError::Stale);
        }
        if self.flows.values().any(|flow| flow.terminal.is_none()) {
            return Err(CodexError::Uncertain);
        }
        // Keep at most the current flow/last terminal reply, not a lifetime auth journal.
        self.flows.clear();
        let kind = match method {
            "browser" => "chatgpt",
            "device" => "chatgptDeviceCode",
            _ => return Err(CodexError::Unsupported),
        };
        // Retain uncertainty BEFORE delivery. Never restart a lost-ack login implicitly.
        self.flows.insert(
            flow_id.to_owned(),
            AuthFlow {
                login_id: String::new(),
                completion: None,
                terminal: None,
            },
        );
        let mut result = self
            .server
            .call("account/login/start", json!({"type":kind}))?;
        let login_id = result["loginId"]
            .as_str()
            .ok_or(CodexError::Protocol)?
            .to_owned();
        let url = result["authUrl"]
            .as_str()
            .or_else(|| result["verificationUrl"].as_str())
            .ok_or(CodexError::Protocol)?;
        let interaction = SensitiveAuthInteraction::Browser {
            request_id: flow_id.to_owned(),
            url: url.to_owned(),
            user_code: result["userCode"].as_str().map(str::to_owned),
        };
        crate::transport::wipe_value(&mut result);
        if !interaction.is_valid() || login_id.len() > 256 {
            return Err(CodexError::Bounds);
        }
        self.flows.insert(
            flow_id.to_owned(),
            AuthFlow {
                login_id,
                completion: None,
                terminal: None,
            },
        );
        Ok(HarnessAuthStep {
            flow_id: flow_id.to_owned(),
            status: self.status_value(AuthReadiness::LoginRequired),
            interaction: Some(interaction),
            complete: false,
        })
    }

    /// Poll readiness only; native browser/device completion is owned by Codex.
    ///
    /// # Errors
    /// Unknown/restarted flows are rejected, never recreated.
    pub fn poll(&mut self, flow_id: &str) -> Result<HarnessAuthStep, CodexError> {
        let flow = self.flows.get(flow_id).ok_or(CodexError::Stale)?;
        if let Some(status) = &flow.terminal {
            return Ok(HarnessAuthStep {
                flow_id: flow_id.to_owned(),
                status: status.clone(),
                interaction: None,
                complete: true,
            });
        }
        let login_id = flow.login_id.clone();
        if login_id.is_empty() {
            return Err(CodexError::Uncertain);
        }
        if flow.completion.is_none() {
            let (events, gap, closed) = self.server.events(128)?;
            if gap || closed {
                return Err(CodexError::Uncertain);
            }
            if let Some(event) = events.iter().find(|event| {
                event["method"] == "account/login/completed"
                    && event["params"]["loginId"].as_str() == Some(login_id.as_str())
            }) {
                // Retain terminal evidence before any fallible follow-up status RPC.
                self.flows
                    .get_mut(flow_id)
                    .ok_or(CodexError::Stale)?
                    .completion = event["params"]["success"].as_bool();
            }
        }
        let completion = self.flows.get(flow_id).ok_or(CodexError::Stale)?.completion;
        let Some(success) = completion else {
            return Ok(HarnessAuthStep {
                flow_id: flow_id.to_owned(),
                status: self.status_value(AuthReadiness::LoginRequired),
                interaction: None,
                complete: false,
            });
        };
        // The correlated success is durable only for this live flow. Readiness reinspection
        // may fail, but must not erase this completion and wait for a second notification.
        let status = if success {
            self.status()
                .unwrap_or_else(|_| self.status_value(AuthReadiness::Unknown))
        } else {
            self.status_value(AuthReadiness::Blocked(ManagedAuthFailure::Rejected))
        };
        self.flows
            .get_mut(flow_id)
            .ok_or(CodexError::Stale)?
            .terminal = Some(status.clone());
        Ok(HarnessAuthStep {
            flow_id: flow_id.to_owned(),
            status,
            interaction: None,
            complete: true,
        })
    }

    /// Cancel exactly one live native login, fencing identity before send.
    /// `notFound` is returned as blocked/uncertain, never successful cancellation.
    ///
    /// # Errors
    /// Missing native acknowledgment identity is uncertain, not a cancellable fabricated flow.
    pub fn cancel(&mut self, flow_id: &str) -> Result<HarnessAuthStep, CodexError> {
        let flow = self.flows.get(flow_id).ok_or(CodexError::Stale)?;
        if let Some(status) = &flow.terminal {
            return Ok(HarnessAuthStep {
                flow_id: flow_id.to_owned(),
                status: status.clone(),
                interaction: None,
                complete: true,
            });
        }
        let login_id = flow.login_id.clone();
        if login_id.is_empty() {
            return Err(CodexError::Uncertain);
        }
        // Fence before sending; lost ack never causes a second cancel RPC.
        let uncertain = self.status_value(AuthReadiness::Blocked(ManagedAuthFailure::Uncertain));
        self.flows
            .get_mut(flow_id)
            .ok_or(CodexError::Stale)?
            .terminal = Some(uncertain);
        let result = self
            .server
            .call("account/login/cancel", json!({"loginId":login_id}))?;
        let status = match result["status"].as_str() {
            Some("canceled") => self.status_value(AuthReadiness::LoginRequired),
            Some("notFound") => {
                self.status_value(AuthReadiness::Blocked(ManagedAuthFailure::Uncertain))
            }
            _ => return Err(CodexError::Protocol),
        };
        self.flows
            .get_mut(flow_id)
            .ok_or(CodexError::Stale)?
            .terminal = Some(status.clone());
        Ok(HarnessAuthStep {
            flow_id: flow_id.to_owned(),
            status,
            interaction: None,
            complete: true,
        })
    }

    /// Deliver a host-scoped vault key only after native ephemeral storage is verified.
    /// Caller must route this exclusively through sensitive, trace-exempt auth transport.
    ///
    /// # Errors
    /// Rejects native/external custody, unverified ephemeral storage and invalid key bounds.
    pub fn vault_key(&mut self, key: &str) -> Result<ManagedAuthStatus, CodexError> {
        if self.binding.custody != AuthCustodyMode::VaultManaged
            || !self.ephemeral_verified
            || key.is_empty()
            || key.len() > 16 * 1024
        {
            return Err(CodexError::Configuration);
        }
        self.server
            .call("account/login/start", json!({"type":"apiKey","apiKey":key}))?;
        self.status()
    }
}
