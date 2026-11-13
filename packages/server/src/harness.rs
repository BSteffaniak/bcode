//! Host-owned managed external session application/runtime.
//!
//! This module never enters the Bcode model loop or dispatches observed native tools.
//! Mutating native calls are preceded by a canonical, ownership-fenced delivery marker.
//! A dropped future or lost reply therefore leaves reconciliation evidence, not replay authority.

mod activity;
mod admission;
mod application;
mod assistance;
mod auth_inspection;
mod canonical;
pub(crate) mod configuration_views;
pub(crate) mod execution_catalog;
mod history;
mod live;
pub(crate) mod native_policy;
mod observation_watch;

/// Explicit graceful-restart recovery is published. It still requires canonical recovery
/// storage, exact physical release evidence and every admission fence in the recovery route.
pub(crate) const fn recovery_published() -> bool {
    bcode_session_models::external_harness::recovery_storage_available()
}
mod registrations;
pub(crate) use registrations::{
    commit as commit_registration, resolution as registration_resolution,
    snapshot as registration_snapshot,
};
mod policy;
mod runtime;
mod store;
mod tool_invocation;
mod tool_queue;
mod transport;
pub(crate) use observation_watch::ObservationWatches;
pub use tool_invocation::{ManagedToolBridge, ManagedToolEndpoint};
pub(crate) use tool_queue::ToolQueueHost;
pub(crate) use transport::with_credential_admission;

#[cfg(test)]
mod auth_status_tests;
#[cfg(test)]
mod canonical_tests;
#[cfg(test)]
mod tests;

pub use admission::{CanonicalManagedAdmission, install_canonical_backend};
pub use application::ManagedHarnessApplication;
pub(crate) use application::drain_delegated_observations;
pub use assistance::ParentAssistanceAuthorization;
pub use auth_inspection::{
    HarnessAuthAdmissionEvidence, HarnessAuthExecution, HarnessAuthMethodEvidence,
    HarnessNativeLoginState, InspectedHarnessAuth,
};
pub use canonical::CanonicalHarnessStore;
pub use history::canonical_history;
pub use runtime::ManagedHarnessRuntime;
pub use store::{DeliveryClaim, HarnessOwner, ManagedHarnessStore, ManagedSession};
pub use transport::{
    HarnessPluginTransport, ManagedHarnessError, ManagedHarnessTransport,
    SensitiveHarnessAuthRequest,
};
