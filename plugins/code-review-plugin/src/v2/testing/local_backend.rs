//! In-process [`ReviewBackend`] serving the real V2 data layer, for surface tests and
//! benchmarks without a daemon.
//!
//! Annotation and workspace operations (drafts, threads, AI exchanges, suggestions, workspace
//! updates) go to the plugin's real persisted store in a private state directory, so surface
//! tests exercise the same data V1 reads. Sessions are recorded instead of created.

use crate::v2::provider::ProviderCallError;
use crate::v2::service::V2Service;
use crate::v2::tui::backend::{BackendFuture, ReviewBackend};
use bcode_code_review_models::{CODE_REVIEW_SERVICE_INTERFACE_ID, REVIEW_SOURCE_ERROR_FAILED};
use bcode_plugin_sdk::prelude::ServiceRequest;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Serves `bcode.code_review/v1` data-layer operations with an in-process [`V2Service`]. No
/// review-source providers are enabled.
pub struct LocalBackend {
    service: Mutex<V2Service>,
    calls: AtomicUsize,
    state: tempfile::TempDir,
    sessions: Mutex<Vec<(String, String)>>,
    messages: Mutex<Vec<(String, String)>>,
    operations: Mutex<Vec<String>>,
    fail: Mutex<std::collections::BTreeSet<String>>,
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self {
            service: Mutex::default(),
            calls: AtomicUsize::new(0),
            state: tempfile::tempdir().expect("state directory"),
            sessions: Mutex::default(),
            messages: Mutex::default(),
            operations: Mutex::default(),
            fail: Mutex::default(),
        }
    }
}

impl std::fmt::Debug for LocalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalBackend")
            .field("calls", &self.calls())
            .finish_non_exhaustive()
    }
}

impl LocalBackend {
    /// Service calls answered so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Sessions created so far: `(id, title)`.
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    #[must_use]
    pub fn sessions(&self) -> Vec<(String, String)> {
        self.sessions.lock().expect("sessions lock").clone()
    }

    /// Messages sent so far: `(session id, text)`.
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    #[must_use]
    pub fn messages(&self) -> Vec<(String, String)> {
        self.messages.lock().expect("messages lock").clone()
    }

    /// Code review operations called so far, oldest first.
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    #[must_use]
    pub fn operations(&self) -> Vec<String> {
        self.operations.lock().expect("operations lock").clone()
    }

    /// Make every later call of `operation` fail (or succeed again when `fail` is false).
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    pub fn set_failing(&self, operation: &str, fail: bool) {
        let mut failing = self.fail.lock().expect("fail lock");
        if fail {
            failing.insert(operation.to_owned());
        } else {
            failing.remove(operation);
        }
    }

    /// Call one operation directly (for test setup and assertions), returning its JSON payload.
    ///
    /// # Panics
    ///
    /// Panics when the operation fails or is unknown.
    #[must_use]
    pub fn call_now<Req: serde::Serialize, Resp: serde::de::DeserializeOwned>(
        &self,
        operation: &str,
        request: &Req,
    ) -> Resp {
        let request = ServiceRequest {
            interface_id: CODE_REVIEW_SERVICE_INTERFACE_ID.to_owned(),
            operation: operation.to_owned(),
            payload: serde_json::to_vec(request).expect("request encodes"),
        };
        let response = self.serve(&request).expect("known operation");
        assert!(
            response.error.is_none(),
            "{operation}: {:?}",
            response.error
        );
        serde_json::from_slice(&response.payload).expect("response decodes")
    }

    fn config(&self) -> crate::CodeReviewPluginConfig {
        crate::CodeReviewPluginConfig {
            state_location: crate::CodeReviewStateLocation::User,
            state_dir: Some(self.state.path().to_path_buf()),
        }
    }

    fn serve(
        &self,
        request: &ServiceRequest,
    ) -> Option<bcode_plugin_sdk::prelude::ServiceResponse> {
        self.service
            .lock()
            .expect("service lock")
            .invoke(request)
            .or_else(|| crate::invoke_stateful_for_tests(request, &self.config()))
    }
}

impl ReviewBackend for LocalBackend {
    fn call_review(&self, operation: String, payload: Vec<u8>) -> BackendFuture<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.operations
            .lock()
            .expect("operations lock")
            .push(operation.clone());
        let failing = self.fail.lock().expect("fail lock").contains(&operation);
        let response = if failing {
            Some(bcode_plugin_sdk::prelude::ServiceResponse::error(
                "test_failure",
                format!("{operation} failed (test)"),
            ))
        } else {
            self.serve(&ServiceRequest {
                interface_id: CODE_REVIEW_SERVICE_INTERFACE_ID.to_owned(),
                operation: operation.clone(),
                payload,
            })
        };
        Box::pin(async move {
            let Some(response) = response else {
                return Err(ProviderCallError {
                    code: REVIEW_SOURCE_ERROR_FAILED.to_owned(),
                    message: format!("{operation} is not a V2 data-layer operation"),
                });
            };
            match response.error {
                Some(error) => Err(ProviderCallError {
                    code: error.code,
                    message: error.message,
                }),
                None => Ok(response.payload),
            }
        })
    }

    fn create_session(
        &self,
        title: String,
        _working_directory: std::path::PathBuf,
    ) -> BackendFuture<String> {
        let id = bcode_session_models::SessionId::new().to_string();
        self.sessions
            .lock()
            .expect("sessions lock")
            .push((id.clone(), title));
        Box::pin(async move { Ok(id) })
    }

    fn send_message(&self, session_id: String, text: String) -> BackendFuture<()> {
        self.messages
            .lock()
            .expect("messages lock")
            .push((session_id, text));
        Box::pin(async { Ok(()) })
    }

    fn plugins_serving(&self, _interface_id: String) -> BackendFuture<Vec<String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn invoke(
        &self,
        plugin_id: String,
        _interface_id: String,
        operation: String,
        _payload: Vec<u8>,
    ) -> BackendFuture<Vec<u8>> {
        Box::pin(async move {
            Err(ProviderCallError {
                code: REVIEW_SOURCE_ERROR_FAILED.to_owned(),
                message: format!("no provider {plugin_id} for {operation}"),
            })
        })
    }
}
