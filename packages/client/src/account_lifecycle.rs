//! A dedicated, caller-bound account lifecycle conversation.

use super::{BcodeClient, ClientConnection, ClientError, Request, ResponsePayload};

/// Keeps reviewed account plans bound to one daemon connection until completion.
///
/// Dropping this handle cancels its pending plans. Transport failure or cancellation
/// invalidates the handle; it never reconnects or replays an ambiguous operation.
pub struct AccountLifecycleClient {
    connection: tokio::sync::Mutex<Option<ClientConnection>>,
    timeout: std::time::Duration,
}

impl BcodeClient {
    /// Open a dedicated conversation for catalog, preview, confirmation and execution.
    ///
    /// # Errors
    /// Fails if the daemon connection or identity verification fails.
    pub async fn account_lifecycle(&self) -> Result<AccountLifecycleClient, ClientError> {
        let mut connection = self.connect("bcode-account-lifecycle").await?;
        connection.reconnect_client = None;
        Ok(AccountLifecycleClient {
            connection: tokio::sync::Mutex::new(Some(connection)),
            timeout: self.request_timeout,
        })
    }
}

impl AccountLifecycleClient {
    async fn send_request(&self, request: Request) -> Result<ResponsePayload, ClientError> {
        let mut slot = self.connection.lock().await;
        // Take before awaiting so cancellation drops the socket, not just the future.
        let mut connection = slot.take().ok_or_else(|| {
            ClientError::Protocol(
                "account lifecycle connection lost; preview again in a new conversation".into(),
            )
        })?;
        let result = tokio::time::timeout(self.timeout, connection.send_lane_request(request))
            .await
            .map_err(|_| ClientError::RequestTimeout {
                timeout: self.timeout,
            })?;
        if result.is_ok() || matches!(&result, Err(ClientError::Server { .. })) {
            *slot = Some(connection);
        }
        result
    }
}

impl AccountLifecycleClient {
    /// Read registered providers and owned declared/runtime accounts without credentials.
    /// # Errors
    /// Fails on transport, caller configuration, or authoritative registry errors.
    pub async fn account_lifecycle_catalog(
        &self,
    ) -> Result<bcode_provider_auth_models::application::AccountLifecycleCatalog, ClientError> {
        match self.send_request(Request::AccountLifecycleCatalog).await? {
            ResponsePayload::AccountLifecycleCatalog { catalog } => Ok(catalog),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Review explicit logout/removal effects; this does not mutate account state.
    /// # Errors
    /// Fails for invalid ownership, unsupported revocation, or unavailable state.
    pub async fn preview_account_lifecycle(
        &self,
        request: bcode_provider_auth_models::account_lifecycle::AccountLifecycleRequest,
        revoke: bool,
    ) -> Result<bcode_provider_auth_models::application::AccountLifecyclePlan, ClientError> {
        match self
            .send_request(Request::PreviewAccountLifecycle { request, revoke })
            .await?
        {
            ResponsePayload::AccountLifecyclePreview { plan } => Ok(plan),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Execute a caller-bound reviewed plan after the caller has confirmed its effects.
    /// # Errors
    /// Fails for unavailable, expired, foreign, or stale plans, or transport errors.
    pub async fn execute_account_lifecycle(
        &self,
        plan_id: String,
    ) -> Result<bcode_provider_auth_models::application::AccountLifecycleExecution, ClientError>
    {
        match self
            .send_request(Request::ExecuteAccountLifecycle { plan_id })
            .await?
        {
            ResponsePayload::AccountLifecycleExecuted { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }
}
