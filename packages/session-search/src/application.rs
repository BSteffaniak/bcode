//! Portable federated search and explicit derived-state maintenance access.

use std::future::Future;

/// Search response with optional bounded canonical hydration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApplicationSearchResult {
    /// Provider-neutral federated result and coverage.
    pub response: crate::FederatedSessionSearchResponse,
    /// Hydrated hits, absent when hydration was not requested.
    pub hydrated_hits: Vec<crate::HydratedSessionSearchHit>,
}

/// Extensible secret-safe failure; does not imply maintenance rollback.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchApplicationFailure {
    /// Stable diagnostic code.
    pub code: String,
    /// Normalized public message.
    pub message: String,
}

/// Caller-bound domain search, not direct provider or persistence access.
pub trait SearchApplication: Sync {
    /// Discover enabled providers without repairing their derived state.
    fn search_providers(
        &self,
    ) -> impl Future<
        Output = Result<crate::ListSessionSearchProvidersResponse, SearchApplicationFailure>,
    > + Send;
    /// Explain deterministic routing without executing searches.
    fn explain_search(
        &self,
        request: crate::SessionSearchRequest,
        policy: crate::SessionSearchPlanPolicy,
        routes: Vec<crate::SessionSearchContentRoute>,
    ) -> impl Future<Output = Result<crate::SessionSearchPlan, SearchApplicationFailure>> + Send;
    /// Execute bounded non-mutating search with optional bounded hydration.
    fn search(
        &self,
        request: crate::SessionSearchRequest,
        policy: crate::SessionSearchPlanPolicy,
        routes: Vec<crate::SessionSearchContentRoute>,
        hydrate: bool,
    ) -> impl Future<Output = Result<ApplicationSearchResult, SearchApplicationFailure>> + Send;
    /// Explicitly purge the selected provider's complete derived search state.
    /// The exact provider confirmation is checked before mutation; this is not a search page.
    /// The response reports maintenance results, not rollback or a resumable operation.
    /// Dropping the request does not prove that purge was cancelled.
    fn purge_search(
        &self,
        provider_id: String,
        confirmation: String,
    ) -> impl Future<
        Output = Result<crate::SessionSearchMaintenanceResponse, SearchApplicationFailure>,
    > + Send;
    /// Explicitly rebuild the selected provider's derived state with required confirmation.
    /// This delegates the provider maintenance operation rather than implicitly repairing reads.
    /// Backfill admission/status/wait/cancel below are the distinct progress-bearing lifecycle;
    /// rebuild itself does not manufacture a cancellation handle or terminal backfill result.
    fn rebuild_search(
        &self,
        provider_id: String,
        confirmation: String,
    ) -> impl Future<
        Output = Result<crate::SessionSearchMaintenanceResponse, SearchApplicationFailure>,
    > + Send;
    /// Admit explicitly requested complete backfill; does not report terminal completion.
    fn start_complete_search_backfill(
        &self,
        request: crate::CompleteSessionSearchBackfillRequest,
    ) -> impl Future<
        Output = Result<crate::StartSessionSearchBackfillResponse, SearchApplicationFailure>,
    > + Send;
    /// Admit one explicit incremental backfill.
    fn start_search_backfill(
        &self,
        request: crate::BackfillSessionSearchRequest,
    ) -> impl Future<
        Output = Result<crate::StartSessionSearchBackfillResponse, SearchApplicationFailure>,
    > + Send;
    /// Inspect one retained transient operation; unavailable does not retry it.
    fn search_backfill_status(
        &self,
        operation_id: String,
    ) -> impl Future<
        Output = Result<crate::SessionSearchBackfillOperationStatus, SearchApplicationFailure>,
    > + Send;
    /// Wait for a revision with the existing bounded wait policy.
    fn wait_search_backfill(
        &self,
        operation_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> impl Future<
        Output = Result<crate::SessionSearchBackfillOperationStatus, SearchApplicationFailure>,
    > + Send;
    /// Request cancellation through the operation owner; acknowledgement is not rollback.
    fn cancel_search_backfill(
        &self,
        operation_id: String,
    ) -> impl Future<
        Output = Result<crate::SessionSearchBackfillOperationStatus, SearchApplicationFailure>,
    > + Send;
    /// Execute one explicit bounded synchronous backfill page.
    fn backfill_search(
        &self,
        request: crate::BackfillSessionSearchRequest,
    ) -> impl Future<Output = Result<crate::SessionSearchBackfillResponse, SearchApplicationFailure>>
    + Send;
}
