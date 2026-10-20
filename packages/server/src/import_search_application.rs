//! Caller-bound import and federated search delegation to existing domain owners.

use crate::ServerState;
use bcode_session_import::application::{
    ApplicationImportRequest, ApplicationImportResult, ImportApplication, ImportApplicationFailure,
};
use bcode_session_models::ClientId;
use bcode_session_search::application::{
    ApplicationSearchResult, SearchApplication, SearchApplicationFailure,
};
use std::sync::Arc;

/// Import/search access supplied by the host after caller context verification.
pub struct ImportSearchApplicationHandle<'a> {
    state: &'a Arc<ServerState>,
    client_id: ClientId,
}
impl<'a> ImportSearchApplicationHandle<'a> {
    /// Bind an already verified caller; does not create authority or canonical state.
    #[must_use]
    pub const fn new(state: &'a Arc<ServerState>, client_id: ClientId) -> Self {
        Self { state, client_id }
    }
    fn require_running(&self) -> Result<(), (&'static str, &'static str)> {
        if self
            .state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(("application_stopped", "application is shutting down"));
        }
        Ok(())
    }
    async fn import_service<T: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        operation: &str,
        request: T,
    ) -> Result<R, ImportApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| ImportApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        let payload = serde_json::to_vec(&request).map_err(|_| {
            import_failure(
                "import_invalid_request",
                "import request could not be encoded",
            )
        })?;
        let response = crate::session_import::call_import_service(self.state, operation, &payload)
            .await
            .map_err(|error| import_failure(error.code, error.message))?;
        if let Some(error) = response.error {
            return Err(import_failure(&error.code, &error.message));
        }
        serde_json::from_slice(&response.payload).map_err(|_| {
            import_failure(
                "import_invalid_response",
                "import source response is incompatible",
            )
        })
    }
    fn search_running(&self) -> Result<(), SearchApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| SearchApplicationFailure {
                code: code.into(),
                message: message.into(),
            })
    }
}
fn import_failure(code: &str, message: &str) -> ImportApplicationFailure {
    ImportApplicationFailure {
        code: code.into(),
        message: message.into(),
    }
}
fn search_failure(
    error: &bcode_session_search::SessionSearchServiceError,
) -> SearchApplicationFailure {
    let code = serde_json::to_value(error.code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "search_failed".into());
    SearchApplicationFailure {
        code,
        message: "session search operation failed; inspect provider status before retrying".into(),
    }
}
impl ImportApplication for ImportSearchApplicationHandle<'_> {
    async fn import_sources(
        &self,
    ) -> Result<bcode_session_import::ListImportSourcesResponse, ImportApplicationFailure> {
        self.import_service(
            bcode_session_import::OP_LIST_IMPORT_SOURCES,
            serde_json::json!({}),
        )
        .await
    }
    async fn discover_importable_sessions(
        &self,
        request: bcode_session_import::DiscoverImportableSessionsRequest,
    ) -> Result<bcode_session_import::DiscoverImportableSessionsResponse, ImportApplicationFailure>
    {
        self.import_service(
            bcode_session_import::OP_DISCOVER_IMPORTABLE_SESSIONS,
            request,
        )
        .await
    }
    async fn load_importable_session(
        &self,
        request: bcode_session_import::LoadImportableSessionRequest,
    ) -> Result<bcode_session_import::ImportableSession, ImportApplicationFailure> {
        self.import_service(bcode_session_import::OP_LOAD_IMPORTABLE_SESSION, request)
            .await
    }
    async fn import_session(
        &self,
        request: ApplicationImportRequest,
    ) -> Result<ApplicationImportResult, ImportApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| import_failure(code, message))?;
        if !request.working_directory.is_absolute() {
            return Err(import_failure(
                "invalid_working_directory",
                "import working directory must be absolute",
            ));
        }
        let (session_id, warnings) = crate::session_import::import_external_session(
            self.state,
            &request.source_id,
            &request.external_session_id,
            &request.working_directory,
        )
        .await
        .map_err(|_| {
            import_failure(
                "session_import_failed",
                "session import failed; inspect canonical state before retrying",
            )
        })?;
        crate::session_operations::require_selected_location(self.state, session_id)
            .await
            .map_err(|error| import_failure(&error.code, &error.message))?;
        if self
            .state
            .active_session_namespace_mismatch(session_id, self.client_id)
            .await
            .is_some()
        {
            return Err(import_failure(
                "session_incompatible_active_client",
                "imported session is active for an incompatible client",
            ));
        }
        let session = self
            .state
            .sessions
            .session_summary(session_id)
            .await
            .map_err(|_| {
                import_failure(
                    "session_import_failed",
                    "imported canonical summary is unavailable",
                )
            })?;
        self.state
            .session_catalog
            .upsert_native_session(session.clone())
            .await;
        Ok(ApplicationImportResult { session, warnings })
    }
}
impl SearchApplication for ImportSearchApplicationHandle<'_> {
    async fn search_providers(
        &self,
    ) -> Result<bcode_session_search::ListSessionSearchProvidersResponse, SearchApplicationFailure>
    {
        self.search_running()?;
        Ok(crate::session_search::list_providers(self.state).await)
    }
    async fn explain_search(
        &self,
        request: bcode_session_search::SessionSearchRequest,
        policy: bcode_session_search::SessionSearchPlanPolicy,
        routes: Vec<bcode_session_search::SessionSearchContentRoute>,
    ) -> Result<bcode_session_search::SessionSearchPlan, SearchApplicationFailure> {
        self.search_running()?;
        Ok(
            bcode_session_search::plan_session_search_with_policy_and_routes(
                &request,
                crate::session_search::list_providers(self.state).await,
                &policy,
                &routes,
            ),
        )
    }
    async fn search(
        &self,
        request: bcode_session_search::SessionSearchRequest,
        policy: bcode_session_search::SessionSearchPlanPolicy,
        routes: Vec<bcode_session_search::SessionSearchContentRoute>,
        hydrate: bool,
    ) -> Result<ApplicationSearchResult, SearchApplicationFailure> {
        self.search_running()?;
        crate::session_search_operations::search(self.state, &request, &policy, &routes, hydrate)
            .await
            .map_err(|error| search_failure(&error))
    }
    async fn purge_search(
        &self,
        provider_id: String,
        confirmation: String,
    ) -> Result<bcode_session_search::SessionSearchMaintenanceResponse, SearchApplicationFailure>
    {
        self.search_running()?;
        crate::session_search::purge_provider(self.state, &provider_id, confirmation)
            .await
            .map_err(|error| search_failure(&error))
    }
    async fn rebuild_search(
        &self,
        provider_id: String,
        confirmation: String,
    ) -> Result<bcode_session_search::SessionSearchMaintenanceResponse, SearchApplicationFailure>
    {
        self.search_running()?;
        crate::session_search::rebuild_provider(self.state, &provider_id, confirmation)
            .await
            .map_err(|error| search_failure(&error))
    }
    async fn start_complete_search_backfill(
        &self,
        request: bcode_session_search::CompleteSessionSearchBackfillRequest,
    ) -> Result<bcode_session_search::StartSessionSearchBackfillResponse, SearchApplicationFailure>
    {
        self.search_running()?;
        crate::start_complete_session_search_backfill(Arc::clone(self.state), request)
            .await
            .map_err(|error| search_failure(&error))
    }
    async fn start_search_backfill(
        &self,
        request: bcode_session_search::BackfillSessionSearchRequest,
    ) -> Result<bcode_session_search::StartSessionSearchBackfillResponse, SearchApplicationFailure>
    {
        self.search_running()?;
        crate::start_session_search_backfill(Arc::clone(self.state), request)
            .await
            .map_err(|error| search_failure(&error))
    }
    async fn search_backfill_status(
        &self,
        operation_id: String,
    ) -> Result<bcode_session_search::SessionSearchBackfillOperationStatus, SearchApplicationFailure>
    {
        self.search_running()?;
        crate::session_search_backfill_status(self.state, &operation_id)
            .await
            .map_err(|error| search_failure(&error))
    }
    async fn wait_search_backfill(
        &self,
        operation_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<bcode_session_search::SessionSearchBackfillOperationStatus, SearchApplicationFailure>
    {
        self.search_running()?;
        crate::wait_session_search_backfill(self.state, &operation_id, after_revision, timeout_ms)
            .await
            .map_err(|error| search_failure(&error))
    }
    async fn cancel_search_backfill(
        &self,
        operation_id: String,
    ) -> Result<bcode_session_search::SessionSearchBackfillOperationStatus, SearchApplicationFailure>
    {
        self.search_running()?;
        crate::cancel_session_search_backfill(self.state, &operation_id)
            .await
            .map_err(|error| search_failure(&error))
    }
    async fn backfill_search(
        &self,
        request: bcode_session_search::BackfillSessionSearchRequest,
    ) -> Result<bcode_session_search::SessionSearchBackfillResponse, SearchApplicationFailure> {
        self.search_running()?;
        crate::session_search::backfill_provider(self.state, request)
            .await
            .map_err(|error| search_failure(&error))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn search_failure_preserves_category_without_provider_diagnostics() {
        let error = super::search_failure(&bcode_session_search::SessionSearchServiceError {
            code: bcode_session_search::SearchErrorCode::FutureVersion,
            message: "secret-provider-output".into(),
            retryable: false,
        });
        assert_eq!(error.code, "future_version");
        assert!(!error.message.contains("secret-provider-output"));
    }
}
