//! Portable catalog observations for model-provider and managed-harness execution.
//!
//! These are nonsecret routing facts, not credentials, dispatch authorization, or proof that a
//! native process accepted a request. Adapters own native discovery and interpretation.

use serde::{Deserialize, Serialize};

/// Execution backend identity. A harness identity is never a model-provider plugin identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionRoute {
    /// Existing Bcode model-provider service.
    ModelProvider {
        /// Provider plugin selected by application configuration.
        provider_plugin_id: String,
    },
    /// Configured external harness instance, not a provider service.
    ManagedHarness {
        /// Exact configured instance, including its executable/configuration namespace.
        instance_id: String,
        /// Plugin owning native protocol interpretation.
        plugin_id: String,
    },
}

/// Billing route reported by the adapter; independent of credential custody.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionBillingRoute {
    /// Billing has not been established.
    Unknown,
    /// Bcode's configured provider API account.
    ProviderApi,
    /// Native harness subscription billing; this does not independently attest credential custody.
    NativeSubscription,
    /// API billing, including a vault-backed managed-harness route. This does not establish
    /// credential ownership or authorize passing Bcode credentials to a native process.
    Api,
}

/// Origin of model membership, distinct from successful native execution evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionModelProvenance {
    /// Membership resolved through the existing provider catalog policy.
    ProviderCatalog,
    /// Model explicitly advertised by the configured harness adapter.
    HarnessDiscovery,
    /// Configuration alone, not evidence that this model is available.
    Configuration,
}

/// Adapter-reported readiness of this exact route/account/model combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionModelReadiness {
    /// Exact route/account/model access has been confirmed; still not submission authorization
    /// or proof of the effective model for a future inference. Discovery alone is insufficient.
    Ready,
    /// Current route and authentication are ready for this exact instance/account. Model access
    /// is separate: advertisement and successful native login do not establish entitlement.
    RouteReady {
        /// Account access evidence for this exact model, not inferred from discovered membership.
        model_access: ExecutionModelAccess,
    },
    /// The adapter reports that this selection cannot currently execute.
    Unavailable,
    /// Readiness has not been established and must not be treated as ready.
    Unknown,
}

/// Model-access evidence independent of native route/auth readiness and effective inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionModelAccess {
    /// No account/model access evidence; an authenticated discovered route may still be attempted.
    Unknown,
    /// Authoritative evidence establishes access for this exact account/model combination.
    Confirmed,
    /// Authoritative evidence excludes access; do not attempt or silently substitute another model.
    Unavailable,
}

/// Central resolver's attempt eligibility, not permission, a receipt or an inference outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionAttemptEligibility {
    /// A discovered authenticated route can be attempted; model access remains unverified.
    EligibleToAttempt,
    /// Account/model access evidence exists; effective inference identity remains separate.
    ModelAccessConfirmed,
}

/// Normalized model membership and routing evidence supplied by a trusted adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionModel {
    /// Selectable model identity and portable capability/display metadata.
    pub model: crate::ModelInfo,
    /// Backend to which this exact selection belongs.
    pub route: ExecutionRoute,
    /// Nonsecret account/configuration reference, never a token or native auth payload.
    pub account_reference: Option<String>,
    /// Explicit billing route, independently of the execution backend and credential custody.
    pub billing_route: ExecutionBillingRoute,
    /// Selector to send to the backend. This is requested, not confirmed, model identity.
    pub native_model_id: String,
    /// Exact identity confirmed by native evidence, if supplied. Never inferred from the selector.
    pub confirmed_model_id: Option<String>,
    /// Evidence source for membership.
    pub provenance: ExecutionModelProvenance,
    /// Readiness of this exact selection, with route/auth and model access evidence distinguished.
    pub readiness: ExecutionModelReadiness,
}

/// Exact selection over one normalized execution catalog; no default or fallback is implied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionModelSelection {
    /// Exact selectable model identity. Aliases must first be normalized by catalog policy.
    pub model_id: String,
    /// Optional exact backend constraint. Omission does not resolve ambiguity by priority.
    pub route: Option<ExecutionRoute>,
    /// Optional nonsecret account constraint.
    pub account_reference: Option<String>,
}

/// Selected route retaining the original request separately from native identity evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedExecutionModel {
    /// Original normalized selection; not evidence of native acceptance.
    pub requested: ExecutionModelSelection,
    /// Exact observed catalog member. Resolution does not execute or authorize a request.
    pub target: ExecutionModel,
    /// Attempt eligibility from the shared resolver, not proof of model access or native acceptance.
    pub eligibility: ExecutionAttemptEligibility,
}

/// Failure to select exactly one ready catalog member. No alternative is selected implicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionSelectionError {
    /// No exact route/account/model member exists.
    NotFound,
    /// Multiple members match, even if only one is ready.
    Ambiguous,
    /// Membership exists but readiness or discovery evidence is insufficient.
    NotReady,
}
