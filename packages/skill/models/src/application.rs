//! Portable skill discovery access.

use std::future::Future;

/// Secret-safe skill application failure.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillApplicationFailure {
    /// Stable diagnostic code.
    pub code: String,
    /// Normalized public message.
    pub message: String,
}

/// Discovery supplied by the owning application without exposing registry internals.
/// Session activation and invocation remain session-owned operations.
pub trait SkillApplication: Sync {
    /// Read the current skill inventory and discovery diagnostics.
    fn list_skills(
        &self,
    ) -> impl Future<Output = Result<crate::SkillList, SkillApplicationFailure>> + Send;
    /// Describe one known skill, rejecting disabled or missing registries.
    fn describe_skill(
        &self,
        skill_id: crate::SkillId,
    ) -> impl Future<Output = Result<crate::SkillManifest, SkillApplicationFailure>> + Send;
}
