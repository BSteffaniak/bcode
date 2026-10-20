//! Caller-bound model inventory and resolution diagnostics.

use std::future::Future;

/// Secret-safe model application failure.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelApplicationFailure {
    /// Stable diagnostic code.
    pub code: String,
    /// Normalized public message.
    pub message: String,
}

/// Resolved user-visible models, retaining the selected provider identity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApplicationModelList {
    /// Provider selected explicitly or through the caller's effective configuration.
    pub provider_plugin_id: Option<String>,
    /// Normalized user-visible inventory from the canonical resolver.
    pub models: bcode_model::ModelList,
}

/// A bounded caller-owned model check; consent is explicit and never inferred from discovery.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApplicationModelCheck {
    /// Caller-owned identity used for cancellation.
    pub check_id: String,
    /// Optional session selection context, verified before provider access.
    pub session_id: Option<bcode_session_models::SessionId>,
    /// Explicit provider override, otherwise canonical caller/session selection.
    pub provider_plugin_id: Option<String>,
    /// Bounded requested model identities.
    pub model_ids: Vec<String>,
    /// Per-check bounded timeout.
    pub timeout_seconds: u64,
    /// Explicit consent for provider spending.
    pub allow_provider_requests: bool,
}

/// Model queries through the existing catalog and provider resolution owner.
/// Session model mutation remains owned by the session application contract.
pub trait ModelApplication: Sync {
    /// Read the complete current catalog document, not a bounded model-list page.
    /// # Errors
    /// Fails if the owning application is unavailable.
    fn model_catalog_snapshot(
        &self,
    ) -> impl Future<
        Output = Result<bcode_model_catalog_models::CatalogDocument, ModelApplicationFailure>,
    > + Send;
    /// Explicitly refresh remote catalog/cache data and return canonical diagnostics.
    /// Refresh failures remain visible in diagnostics; this does not perform model checks.
    /// # Errors
    /// Fails if the application is unavailable. Dropping the request does not imply rollback.
    fn refresh_model_catalog(
        &self,
    ) -> impl Future<
        Output = Result<
            bcode_model_catalog_models::ModelCatalogDiagnostics,
            ModelApplicationFailure,
        >,
    > + Send;
    /// Resolve user-visible models for the selected provider, applying existing ignore rules.
    fn list_models(
        &self,
        provider_plugin_id: Option<String>,
    ) -> impl Future<Output = Result<ApplicationModelList, ModelApplicationFailure>> + Send;
    /// Resolve models using the canonical session selection and caller provider context.
    fn session_model_list(
        &self,
        session_id: bcode_session_models::SessionId,
        provider_plugin_id: Option<String>,
    ) -> impl Future<Output = Result<ApplicationModelList, ModelApplicationFailure>> + Send;
    /// Run bounded provider checks only with explicit consent.
    fn check_models(
        &self,
        request: ApplicationModelCheck,
    ) -> impl Future<Output = Result<bcode_model::ModelCheckReport, ModelApplicationFailure>> + Send;
    /// Request cancellation of this caller's check; acceptance is not terminal proof.
    fn cancel_model_check(
        &self,
        check_id: String,
    ) -> impl Future<Output = Result<bool, ModelApplicationFailure>> + Send;
    /// Inspect a canonical session's model under this caller's runtime context.
    fn session_model_status(
        &self,
        session_id: bcode_session_models::SessionId,
    ) -> impl Future<Output = Result<bcode_model::SessionModelStatus, ModelApplicationFailure>> + Send;
    /// Inspect the caller's configured default resolution, not an ad hoc alias lookup.
    fn default_model_status(
        &self,
    ) -> impl Future<Output = Result<bcode_model::SessionModelStatus, ModelApplicationFailure>> + Send;
    /// Return normalized catalog diagnostics without provider-private representations.
    fn model_catalog_diagnostics(
        &self,
    ) -> impl Future<
        Output = Result<
            bcode_model_catalog_models::ModelCatalogDiagnostics,
            ModelApplicationFailure,
        >,
    > + Send;
}
