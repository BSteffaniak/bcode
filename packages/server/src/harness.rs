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
mod history;
mod policy;
mod runtime;
mod store;
mod tool_invocation;
mod tool_queue;
mod transport;
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
