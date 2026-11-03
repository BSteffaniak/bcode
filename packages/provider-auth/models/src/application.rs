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

/// Provider-neutral account inventory owned by the authenticated application's configuration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccountLifecycleCatalog {
    /// Registered providers, including their owned declared and runtime accounts.
    pub providers: Vec<AccountLifecycleProvider>,
}

/// Registered plugin ownership and available account methods.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccountLifecycleProvider {
    /// Provider ID, distinct from the owning plugin ID.
    pub provider_id: String,
    /// Owning registered plugin.
    pub owner_plugin_id: String,
    /// Provider-authored display name.
    pub display_name: String,
    /// Declared methods and their verification/revocation capabilities.
    pub methods: Vec<crate::AuthMethodContribution>,
    /// Explicitly owned profiles; no credentials are transported.
    pub accounts: Vec<AccountLifecycleAccount>,
}

/// One explicit profile eligible for lifecycle planning.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccountLifecycleAccount {
    /// Explicit profile name.
    pub profile: String,
    /// Configured method, if declared.
    pub method_id: Option<String>,
    /// Whether the profile comes from declarative configuration.
    pub declared: bool,
    /// Local owned credential presence, not remote validity or refreshability.
    #[serde(default)]
    pub credentials: AccountLifecycleCredentials,
}

/// Read-only local custody evidence; never triggers refresh or provider requests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountLifecycleCredentials {
    /// No local evidence was supplied by this server version.
    #[default]
    Unknown,
    /// At least one nonempty owned credential is readable; not proof of authentication.
    Present,
    /// No nonempty owned credentials were found.
    Missing,
    /// Owned custody or its method could not be inspected safely.
    Inaccessible,
}

/// Host-retained, caller-bound reviewed plan. Execution never accepts a rewritten preview.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccountLifecyclePlan {
    /// Opaque single-use daemon plan identity.
    pub plan_id: String,
    /// Shared operation's secret-free local effects.
    pub local: crate::account_lifecycle::AccountLifecyclePreview,
    /// Whether the user requested optional provider revocation.
    pub revoke: bool,
}

/// Independent remote result; failure does not prevent local cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountLifecycleRevocation {
    /// No remote action requested.
    NotRequested,
    /// Provider confirmed revocation.
    Complete,
    /// Provider invocation failed or did not confirm completion.
    Failed,
}

/// Local and remote evidence are deliberately separate.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccountLifecycleExecution {
    /// Shared operation's local effects and normalized failure.
    pub local: crate::account_lifecycle::AccountLifecycleOutcome,
    /// Optional provider-owned revocation result.
    pub revocation: AccountLifecycleRevocation,
}

/// Caller-configuration-bound, secret-free account lifecycle boundary.
pub trait AccountLifecycleApplication: Sync {
    /// Discover registered providers and their explicitly owned profiles.
    /// # Errors
    /// Fails when the caller configuration or runtime registry is unavailable.
    fn account_lifecycle_catalog(
        &self,
    ) -> impl Future<Output = Result<AccountLifecycleCatalog, AuthPoolApplicationFailure>> + Send;
    /// Preview local effects and retain an immutable plan for this caller.
    /// # Errors
    /// Fails for invalid ownership, unsupported revocation, or unavailable state.
    fn preview_account_lifecycle(
        &self,
        request: crate::account_lifecycle::AccountLifecycleRequest,
        revoke: bool,
    ) -> impl Future<Output = Result<AccountLifecyclePlan, AuthPoolApplicationFailure>> + Send;
    /// Execute a reviewed single-use plan, revalidating authoritative state.
    /// # Errors
    /// Fails for an expired, foreign, stale, or unavailable plan.
    fn execute_account_lifecycle(
        &self,
        plan_id: String,
    ) -> impl Future<Output = Result<AccountLifecycleExecution, AuthPoolApplicationFailure>> + Send;
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
