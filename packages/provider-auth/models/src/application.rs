//! Secret-free caller-bound auth-pool application access.

use std::future::Future;

/// Extensible public failure; unknown codes never imply success.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuthPoolApplicationFailure {
    /// Machine-readable category.
    pub code: String,
    /// Secret-safe diagnostic.
    pub message: String,
}

/// Effective pool preference, not credential material or declarative configuration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuthPoolPreference {
    /// Verified pool name.
    pub pool: String,
    /// Effective preferred profile, absent when none is selected.
    pub profile: Option<String>,
}

/// Session-bound account selection and configuration-scoped account metadata operations.
pub trait AccountRoutingApplication: Sync {
    /// Read account routing bound to a session's configuration.
    /// # Errors
    /// Fails for unavailable sessions or configuration state.
    fn account_routing(
        &self,
        session_id: String,
    ) -> impl Future<Output = Result<crate::AccountRouting, AuthPoolApplicationFailure>> + Send;
    /// Change explicit next-turn or new-session routing.
    /// # Errors
    /// Fails for invalid accounts, scope, or persistence failure.
    fn set_account_routing(
        &self,
        session_id: String,
        request: crate::SetAccountRoutingRequest,
    ) -> impl Future<Output = Result<crate::AccountRouting, AuthPoolApplicationFailure>> + Send;
    /// Pause/resume an account throughout the session's configuration.
    /// # Errors
    /// Fails for unknown accounts or persistence failure.
    fn set_account_pause(
        &self,
        session_id: String,
        request: crate::SetAccountPauseRequest,
    ) -> impl Future<Output = Result<crate::AccountRouting, AuthPoolApplicationFailure>> + Send;
    /// Change display metadata without changing credential identity.
    /// # Errors
    /// Fails for unknown accounts, invalid labels, or persistence failure.
    fn rename_account(
        &self,
        session_id: String,
        request: crate::RenameAccountRequest,
    ) -> impl Future<Output = Result<crate::AccountRouting, AuthPoolApplicationFailure>> + Send;
}

/// Auth-owned status and interactive preference access; never transports credentials.
pub trait AuthPoolApplication: Sync {
    /// Read secret-free pool summaries.
    /// # Errors
    /// Fails on unavailable configuration or stopped application.
    fn auth_pool_list(
        &self,
    ) -> impl Future<Output = Result<Vec<crate::AuthPoolSummary>, AuthPoolApplicationFailure>> + Send;
    /// Read an effective preference; an unknown pool is an error, not an empty preference.
    /// # Errors
    /// Fails on unavailable configuration or unknown pool.
    fn auth_pool_preference(
        &self,
        pool: String,
    ) -> impl Future<Output = Result<AuthPoolPreference, AuthPoolApplicationFailure>> + Send;
    /// Set or clear interactive user state without modifying declarative configuration.
    /// # Errors
    /// Fails on invalid pool/profile or preference persistence failure.
    fn apply_auth_pool_preference(
        &self,
        request: crate::SetAuthPoolPreferenceRequest,
    ) -> impl Future<Output = Result<(), AuthPoolApplicationFailure>> + Send;
}
