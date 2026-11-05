//! Dedicated, ephemeral managed auth channel. Sensitive material has no serializable UI wrapper.
#[cfg(unix)]
mod terminal;
#[cfg(all(test, unix))]
mod tests;
use super::attachment::ManagedSessionCommandHandle;
use crate::{ClientConnection, ClientError};
use bcode_ipc::harness::{
    MANAGED_SESSION_VERSION, ManagedSessionAuthRequest, ManagedSessionOperation,
    ManagedSessionRequest,
};
use bcode_provider_auth_models::managed::{
    AuthSensitiveInput, ManagedAuthHandle, ManagedAuthStatus, ManagedFlowSnapshot,
    SensitiveAuthInteraction,
};

/// One authenticated caller for a session's native-owned auth waiter lifecycle.
///
/// Not clonable/serializable; never reconnects or automatically replays login operations.
/// Drop does not assert cancellation or native settlement. Explicitly cancel before dropping
/// a live waiter where possible; after transport loss use readiness inspection, not replay.
pub struct ManagedSessionAuthFlow {
    connection: ClientConnection,
    session_id: bcode_session_models::SessionId,
    instance_id: String,
    namespace: bcode_provider_auth_models::managed::AuthNamespace,
    custody: bcode_provider_auth_models::managed::AuthCustodyMode,
    handle: Option<ManagedAuthHandle>,
    revision: Option<u64>,
    uncertain: bool,
    begin_attempted: bool,
    terminal_attempted: bool,
}

impl super::ManagedSessionClient {
    /// Open a sealed auth caller for an explicitly selected existing managed session/instance.
    /// No attachment, model credentials, login mutation or raw connection is exposed.
    /// # Errors
    /// Returns canonical binding, explicit instance mismatch or connection/configuration errors.
    pub async fn session_auth_flow(
        self,
        session_id: bcode_session_models::SessionId,
        instance_id: &str,
    ) -> Result<ManagedSessionAuthFlow, ClientError> {
        let commands = ManagedSessionCommandHandle {
            client: std::sync::Arc::new(self),
            session_id,
            command_connection: std::sync::Arc::default(),
        };
        let binding = commands.binding().await?;
        if binding.instance.instance_id != instance_id {
            return Err(invalid());
        }
        let flow = commands.auth_flow().await?;
        if flow.instance_id != instance_id {
            return Err(invalid());
        }
        Ok(flow)
    }
}

impl ManagedSessionCommandHandle {
    /// Connect a separate dedicated auth surface for this exact canonical managed session.
    /// No authentication mutation or credential inspection is performed by construction.
    /// # Errors
    /// Returns canonical binding admission, config or connection failures.
    pub async fn auth_flow(&self) -> Result<ManagedSessionAuthFlow, ClientError> {
        let binding = self.binding().await?;
        let connection = self
            .client
            .inner
            .connect("bcode-managed-auth-surface")
            .await?;
        Ok(ManagedSessionAuthFlow {
            connection,
            session_id: self.session_id,
            namespace: bcode_provider_auth_models::managed::AuthNamespace {
                plugin_id: binding.instance.plugin_id.clone(),
                credential_namespace: binding.instance.auth.credential_namespace.clone(),
            },
            custody: binding.instance.auth.custody,
            instance_id: binding.instance.instance_id,
            handle: None,
            revision: None,
            uncertain: false,
            begin_attempted: false,
            terminal_attempted: false,
        })
    }
}

fn invalid() -> ClientError {
    ClientError::Protocol(
        "managed authentication flow unavailable; inspect readiness without replay".into(),
    )
}

impl ManagedSessionAuthFlow {
    async fn request<T: serde::de::DeserializeOwned>(
        &mut self,
        operation: ManagedSessionOperation,
        input: Option<AuthSensitiveInput>,
        sensitive_interaction: bool,
    ) -> Result<T, ClientError> {
        if self.uncertain || self.terminal_attempted {
            return Err(invalid());
        }
        let request = ManagedSessionAuthRequest {
            session_id: self.session_id,
            instance_id: self.instance_id.clone(),
            handle: self.handle.clone(),
            input,
            sensitive_interaction,
        };
        let payload = serde_json::to_string(&request).map_err(|_| invalid())?;
        self.exchange(operation, payload).await
    }

    async fn exchange<T: serde::de::DeserializeOwned>(
        &mut self,
        operation: ManagedSessionOperation,
        request_json: String,
    ) -> Result<T, ClientError> {
        if self.uncertain {
            return Err(invalid());
        }
        // Cancellation of a partial framed exchange leaves this dedicated channel unusable.
        // Clear only after a complete validated reply; never replay a cancelled auth mutation.
        self.uncertain = true;
        let response = match self
            .connection
            .managed_session_auth(ManagedSessionRequest {
                schema_version: MANAGED_SESSION_VERSION,
                operation,
                request_json,
            })
            .await
        {
            Ok(response) => response,
            // Preserve only host-normalized error codes. Auth error messages can
            // carry sensitive provider/native details and must not escape this channel.
            Err(ClientError::Server { code, .. })
                if matches!(
                    code.as_str(),
                    "managed_session_invalid"
                        | "managed_session_storage_unavailable"
                        | "managed_session_stale_owner"
                        | "managed_session_conflict"
                        | "managed_session_uncertain"
                        | "managed_session_unavailable"
                        | "managed_session_deadline"
                        | "managed_session_unsupported"
                ) =>
            {
                return Err(ClientError::Server {
                    code,
                    message: "managed authentication admission or status inspection rejected; no login replayed".into(),
                });
            }
            Err(_) => return Err(invalid()),
        };
        let Ok(value) = serde_json::from_str(&response.response_json) else {
            return Err(invalid());
        };
        self.uncertain = false;
        Ok(value)
    }

    fn accept(
        &mut self,
        snapshot: ManagedFlowSnapshot,
    ) -> Result<ManagedFlowSnapshot, ClientError> {
        if self
            .handle
            .as_ref()
            .is_some_and(|handle| handle != &snapshot.handle)
            || self
                .revision
                .is_some_and(|revision| snapshot.revision < revision)
        {
            self.uncertain = true;
            return Err(invalid());
        }
        self.handle = Some(snapshot.handle.clone());
        self.revision = Some(snapshot.revision);
        Ok(snapshot)
    }

    /// Inspect readiness without beginning, refreshing or resuming native login.
    /// # Errors
    /// Returns existing host auth admission/inspection failures. Use a fresh flow after restart.
    pub async fn readiness(&mut self) -> Result<ManagedAuthStatus, ClientError> {
        if self.handle.is_some() {
            return Err(invalid());
        }
        let status: ManagedAuthStatus = self
            .request(ManagedSessionOperation::AuthStatus, None, false)
            .await?;
        if status.namespace != self.namespace || status.custody != self.custody {
            self.uncertain = true;
            return Err(invalid());
        }
        Ok(status)
    }

    /// Explicitly begin one supported auth flow under native custody and current host admission.
    /// # Errors
    /// Rejects duplicate begin and unsupported methods. Terminal handoff is not silently replaced
    /// with protocol login; Unix callers must explicitly use `terminal_handoff` with terminal ownership.
    pub async fn begin(&mut self) -> Result<ManagedFlowSnapshot, ClientError> {
        if self.handle.is_some() || self.begin_attempted {
            return Err(invalid());
        }
        self.begin_attempted = true;
        let snapshot = self
            .request(ManagedSessionOperation::AuthBegin, None, false)
            .await?;
        self.accept(snapshot)
    }

    /// Read secret-free live flow state; this does not acknowledge or replay a native command.
    /// # Errors
    /// Returns missing/stale waiter, revision, admission or transport failures.
    pub async fn status(&mut self) -> Result<ManagedFlowSnapshot, ClientError> {
        if self.handle.is_none() {
            return Err(invalid());
        }
        let snapshot = self
            .request(ManagedSessionOperation::AuthStatus, None, false)
            .await?;
        self.accept(snapshot)
    }

    /// View sensitive interaction only inside a synchronous dedicated auth renderer callback.
    /// The method does not return or enqueue the interaction. The dedicated renderer must not
    /// copy it into ordinary chat history, persistent UI state, logs or telemetry.
    /// # Errors
    /// Returns missing/stale waiter, admission, decoding or transport failures.
    pub async fn with_sensitive_interaction(
        &mut self,
        render: impl FnOnce(Option<&SensitiveAuthInteraction>),
    ) -> Result<(), ClientError> {
        if self.handle.is_none() {
            return Err(invalid());
        }
        let interaction: Option<SensitiveAuthInteraction> = self
            .request(ManagedSessionOperation::AuthStatus, None, true)
            .await?;
        if interaction.as_ref().is_some_and(|value| !value.is_valid()) {
            self.uncertain = true;
            return Err(invalid());
        }
        render(interaction.as_ref());
        Ok(())
    }

    /// Explicitly deliver sensitive input using existing redacted/zeroizing auth contracts.
    /// # Errors
    /// Returns invalid input, stale waiter, host admission, uncertain delivery or transport errors.
    /// Retrying after uncertainty is not authorized by this method.
    pub async fn continue_with_input(
        &mut self,
        input: AuthSensitiveInput,
    ) -> Result<ManagedFlowSnapshot, ClientError> {
        if self.handle.is_none() || !input.is_valid() {
            return Err(invalid());
        }
        let snapshot = self
            .request(ManagedSessionOperation::AuthContinue, Some(input), false)
            .await?;
        self.accept(snapshot)
    }

    /// Explicitly cancel this caller's waiter. Acknowledgement is not native settlement proof.
    /// # Errors
    /// Returns stale waiter, admission or transport failures; never automatically retries.
    pub async fn cancel(&mut self) -> Result<ManagedFlowSnapshot, ClientError> {
        if self.handle.is_none() {
            return Err(invalid());
        }
        let snapshot = self
            .request(ManagedSessionOperation::AuthCancel, None, false)
            .await?;
        self.accept(snapshot)
    }
}
