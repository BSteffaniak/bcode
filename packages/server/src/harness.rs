//! Host-owned managed external session application/runtime.
//!
//! This module never enters the Bcode model loop or dispatches observed native tools.
//! Mutating native calls are preceded by a canonical, ownership-fenced delivery marker.
//! A dropped future or lost reply therefore leaves reconciliation evidence, not replay authority.

mod admission;
mod application;
mod assistance;
mod auth_inspection;
mod canonical;
pub(crate) mod configuration_views;
pub(crate) mod execution_catalog;
mod history;
mod observation_watch;

/// Recovery remains independently unpublished, regardless of selected-launch writer versions.
pub(crate) const fn recovery_published() -> bool {
    false
}
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
