//! Shared enrollment intent; presentation never decides whether metadata may change.

use super::{EnrollmentDestination, EnrollmentError, EnrollmentPlan, PoolEnrollment};
use bcode_config::{BcodeConfig, RuntimeAuthSubscriptions};
use bcode_provider_auth_models::{AuthPoolCompatibility, AuthProviderContribution};

/// Explicit enrollment intent selected before provider side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollmentIntent {
    /// Renew an owned existing account without changing routing or memberships.
    Reauthenticate,
    /// Create/enroll an account with an explicit membership policy.
    Enroll(PoolEnrollment),
}

/// Plan an enrollment intent using the same validation as ordinary enrollment.
///
/// # Errors
/// Reauthentication rejects missing profiles and all ownership or destination conflicts.
/// Enrollment additionally validates requested pool changes before provider effects.
pub fn plan_intent(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    provider: &AuthProviderContribution,
    registration: (&str, &str),
    destination: EnrollmentDestination,
    intent: EnrollmentIntent,
    compatibility: Option<&AuthPoolCompatibility>,
) -> Result<EnrollmentPlan, EnrollmentError> {
    let (owner, method) = registration;
    let preserve = matches!(intent, EnrollmentIntent::Reauthenticate);
    let pooling = match intent {
        EnrollmentIntent::Reauthenticate => {
            crate::resolve_auth_provider_profile(
                config,
                &provider.provider_id,
                owner,
                destination.profile.as_deref(),
                runtime,
            )?;
            PoolEnrollment::Standalone
        }
        EnrollmentIntent::Enroll(pooling) => pooling,
    };
    let mut plan = super::plan(
        config,
        runtime,
        provider,
        owner,
        method,
        destination,
        pooling,
        compatibility,
    )?;
    plan.preserve_memberships = preserve;
    Ok(plan)
}
