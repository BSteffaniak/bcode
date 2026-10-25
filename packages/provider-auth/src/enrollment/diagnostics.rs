//! Secret-safe enrollment diagnostics shared by every frontend.

use serde::{Deserialize, Serialize};

/// Which non-secret authority contains an enrollment conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentConflictSource {
    /// Declarative configuration.
    Configuration,
    /// Registered runtime metadata.
    Runtime,
    /// Effective configuration and runtime records disagree.
    Combined,
}

/// Field categories only: diagnostics never contain credential values or vault paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentConflictReason {
    /// Provider/plugin ownership cannot be verified.
    Ownership,
    /// Subscription compatibility groups disagree.
    CompatibilityGroup,
    /// More than one pool claims automatic/default intent.
    MultipleDefaults,
    /// A proposed default pool name is already occupied.
    NameCollision,
    /// An existing member uses an incompatible method.
    MemberMethod,
    /// A member is duplicated or has an invalid identity.
    MemberIdentity,
    /// Profile and membership storage/method/sealing records disagree.
    MemberMetadata,
    /// Context-local enrollment may not mutate global pools.
    ContextBoundary,
    /// This method is not eligible for subscription pooling.
    MethodNotEligible,
}

/// Structured, secret-safe failure to change enrollment metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentConflict {
    /// Affected pool, when known.
    pub pool: Option<String>,
    /// Affected account, when known.
    pub profile: Option<String>,
    /// Conflicting authority.
    pub source: EnrollmentConflictSource,
    /// Machine-readable failure category.
    pub reason: EnrollmentConflictReason,
}

impl std::fmt::Display for EnrollmentConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Authentication enrollment conflict")?;
        if let Some(pool) = &self.pool {
            write!(f, " in pool {pool:?}")?;
        }
        if let Some(profile) = &self.profile {
            write!(f, " for account {profile:?}")?;
        }
        let description = match self.reason {
            EnrollmentConflictReason::Ownership => {
                "provider/plugin ownership is missing or inconsistent"
            }
            EnrollmentConflictReason::CompatibilityGroup => {
                "subscription compatibility groups disagree"
            }
            EnrollmentConflictReason::MultipleDefaults => {
                "multiple pools claim automatic/default enrollment"
            }
            EnrollmentConflictReason::NameCollision => {
                "the automatic pool name is already occupied"
            }
            EnrollmentConflictReason::MemberMethod => {
                "an existing member uses an incompatible authentication method"
            }
            EnrollmentConflictReason::MemberIdentity => {
                "a member identity is empty, duplicated, or foreign"
            }
            EnrollmentConflictReason::MemberMetadata => {
                "profile and pool member storage, method, or sealing metadata disagree"
            }
            EnrollmentConflictReason::ContextBoundary => {
                "context-local enrollment cannot change a global pool"
            }
            EnrollmentConflictReason::MethodNotEligible => {
                "this authentication method does not support subscription pooling"
            }
        };
        write!(
            f,
            " ({:?}): {description}. Existing metadata preserved; reauthenticate without changing memberships, or inspect the pool before explicitly changing enrollment.",
            self.source
        )
    }
}
