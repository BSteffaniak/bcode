//! Portable agent discovery and effective policy access.

use std::future::Future;

/// Secret-safe agent application failure.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentApplicationFailure {
    /// Stable diagnostic code.
    pub code: String,
    /// Normalized public message.
    pub message: String,
}

/// Agent metadata access bound to a verified caller's effective configuration.
/// Session agent mutation remains owned by the session application contract.
pub trait AgentApplication: Sync {
    /// List enabled profiles for the caller, rather than daemon defaults.
    fn list_agents(
        &self,
    ) -> impl Future<Output = Result<Vec<crate::AgentInfo>, AgentApplicationFailure>> + Send;
    /// Read the owning plugin's effective policy status.
    fn agent_policy_status(
        &self,
    ) -> impl Future<Output = Result<crate::PolicyStatusResponse, AgentApplicationFailure>> + Send;
}
