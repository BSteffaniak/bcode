//! Service access for the V2 review surface.
//!
//! The surface reaches the code review data layer and the review-source providers through
//! [`ReviewBackend`], an object-safe transport with owned, `Send` futures so calls can run on
//! host-spawned tasks. Production uses the daemon client; tests serve the real V2 service
//! in-process.

use crate::v2::provider::{ProviderCallError, ProviderTransport};
use bcode_code_review_models::{CODE_REVIEW_SERVICE_INTERFACE_ID, REVIEW_SOURCE_ERROR_FAILED};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

/// Owned future returned by [`ReviewBackend`] calls.
pub type BackendFuture<T> = Pin<Box<dyn Future<Output = Result<T, ProviderCallError>> + Send>>;

/// Object-safe access to plugin services for the review surface.
pub trait ReviewBackend: Send + Sync + 'static {
    /// Invoke `operation` of the code review service (`bcode.code_review/v1`).
    fn call_review(&self, operation: String, payload: Vec<u8>) -> BackendFuture<Vec<u8>>;

    /// Ids of enabled plugins serving `interface_id`.
    fn plugins_serving(&self, interface_id: String) -> BackendFuture<Vec<String>>;

    /// Invoke `operation` of `interface_id` on one plugin.
    fn invoke(
        &self,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> BackendFuture<Vec<u8>>;

    /// Create a Bcode session named `title` working in `working_directory`; returns its id.
    fn create_session(&self, title: String, working_directory: PathBuf) -> BackendFuture<String>;

    /// Send `text` to `session_id` as a user message.
    fn send_message(&self, session_id: String, text: String) -> BackendFuture<()>;
}

/// Shared backend handle.
pub type SharedBackend = Arc<dyn ReviewBackend>;

/// Call one code review operation with JSON request and response payloads.
///
/// # Errors
///
/// * The service error, or a decoding error with code `review_source_failed`.
pub async fn call_json<Req: Serialize + Sync, Resp: DeserializeOwned>(
    backend: &dyn ReviewBackend,
    operation: &str,
    request: &Req,
) -> Result<Resp, ProviderCallError> {
    let payload = serde_json::to_vec(request).map_err(|error| ProviderCallError {
        code: REVIEW_SOURCE_ERROR_FAILED.to_owned(),
        message: error.to_string(),
    })?;
    let response = backend.call_review(operation.to_owned(), payload).await?;
    serde_json::from_slice(&response).map_err(|error| ProviderCallError {
        code: REVIEW_SOURCE_ERROR_FAILED.to_owned(),
        message: format!("unexpected {operation} response: {error}"),
    })
}

/// [`ProviderTransport`] over a [`ReviewBackend`], for provider routing.
pub struct BackendTransport<'a>(pub &'a dyn ReviewBackend);

impl ProviderTransport for BackendTransport<'_> {
    fn plugins_serving(
        &self,
        interface_id: &str,
    ) -> impl Future<Output = Result<Vec<String>, ProviderCallError>> + Send {
        self.0.plugins_serving(interface_id.to_owned())
    }

    fn invoke(
        &self,
        plugin_id: &str,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, ProviderCallError>> + Send {
        self.0.invoke(
            plugin_id.to_owned(),
            interface_id.to_owned(),
            operation.to_owned(),
            payload,
        )
    }
}

/// The daemon client backend used in production.
#[derive(Debug, Clone)]
pub struct DaemonBackend {
    client: bcode_client::BcodeClient,
}

impl DaemonBackend {
    /// Backend over the default local daemon endpoint.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: bcode_client::BcodeClient::default_endpoint(),
        }
    }
}

impl Default for DaemonBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn client_error(error: impl std::fmt::Display) -> ProviderCallError {
    ProviderCallError {
        code: REVIEW_SOURCE_ERROR_FAILED.to_owned(),
        message: error.to_string(),
    }
}

impl ReviewBackend for DaemonBackend {
    fn create_session(&self, title: String, working_directory: PathBuf) -> BackendFuture<String> {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .create_session_in_working_directory(Some(title), working_directory)
                .await
                .map(|session| session.id.to_string())
                .map_err(client_error)
        })
    }

    fn send_message(&self, session_id: String, text: String) -> BackendFuture<()> {
        let client = self.client.clone();
        Box::pin(async move {
            let id = session_id
                .parse::<bcode_session_models::SessionId>()
                .map_err(client_error)?;
            client
                .send_user_message(id, text, bcode_session_models::PromptPlacement::FollowUp)
                .await
                .map(|_| ())
                .map_err(client_error)
        })
    }

    fn call_review(&self, operation: String, payload: Vec<u8>) -> BackendFuture<Vec<u8>> {
        let client = self.client.clone();
        Box::pin(async move {
            let response = client
                .call_plugin_service(
                    CODE_REVIEW_SERVICE_INTERFACE_ID.to_owned(),
                    operation,
                    payload,
                )
                .await
                .map_err(|error| ProviderCallError {
                    code: REVIEW_SOURCE_ERROR_FAILED.to_owned(),
                    message: error.to_string(),
                })?;
            match response.error {
                Some(error) => Err(ProviderCallError {
                    code: error.code,
                    message: error.message,
                }),
                None => Ok(response.payload),
            }
        })
    }

    fn plugins_serving(&self, interface_id: String) -> BackendFuture<Vec<String>> {
        let client = self.client.clone();
        Box::pin(async move { ProviderTransport::plugins_serving(&client, &interface_id).await })
    }

    fn invoke(
        &self,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> BackendFuture<Vec<u8>> {
        let client = self.client.clone();
        Box::pin(async move {
            ProviderTransport::invoke(&client, &plugin_id, &interface_id, &operation, payload).await
        })
    }
}
