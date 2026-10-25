//! Validated enrollment plans and post-custody atomic completion.

#[cfg(test)]
mod tests;
use super::{
    EnrollmentConflict, EnrollmentConflictReason as Reason, EnrollmentConflictSource as Source,
    EnrollmentDestination, EnrollmentError, PreparedEnrollment, prepare,
};

fn conflict(
    pool: Option<&str>,
    profile: Option<&str>,
    source: Source,
    reason: Reason,
) -> EnrollmentError {
    EnrollmentError::Conflict(EnrollmentConflict {
        pool: pool.map(str::to_owned),
        profile: profile.map(str::to_owned),
        source,
        reason,
    })
}
use bcode_config::{
    BcodeConfig, RuntimeAuthProfile, RuntimeAuthSubscriptionPool, RuntimeAuthSubscriptionProfile,
    RuntimeAuthSubscriptions,
};
use bcode_provider_auth_models::{AuthPoolCompatibility, AuthProviderContribution};

/// Enrollment intent; explicit pools always take precedence over automatic routing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PoolEnrollment {
    /// Join/create an explicitly marked compatible default pool, if supported by the plugin.
    #[default]
    Automatic,
    /// Join exactly this pool, never an inferred default.
    Explicit(String),
    /// Register only this account; no pool membership changes.
    Standalone,
}

/// Read-only plan; callers must save credentials before invoking completion.
pub struct EnrollmentPlan {
    pub(super) preserve_memberships: bool,
    /// Ownership-checked custody destination.
    pub prepared: PreparedEnrollment,
    expected: RuntimeAuthSubscriptions,
    metadata: RuntimeAuthProfile,
    original: crate::ResolvedAuthProfile,
    pool: Option<(String, RuntimeAuthSubscriptionPool)>,
    result: EnrollmentResult,
    context: bool,
    authorization: serde_json::Value,
}

/// Non-secret publication outcome suitable for all frontends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentResult {
    /// Enrolled account.
    pub profile: String,
    /// Selected pool, if any.
    pub pool: Option<String>,
    /// Whether automatic/default intent selected this pool.
    pub automatic: bool,
    /// Whether completion adds previously absent membership.
    pub joined: bool,
}

fn metadata(resolved: &crate::ResolvedAuthProfile) -> RuntimeAuthProfile {
    RuntimeAuthProfile {
        provider_id: resolved.provider_id.clone(),
        owner_plugin_id: resolved.owner_plugin_id.clone(),
        backend: resolved.profile.backend.clone(),
        scheme: resolved.profile.scheme.clone().unwrap_or_default(),
        storage_profile: resolved
            .profile
            .settings
            .get("profile")
            .cloned()
            .unwrap_or_else(|| resolved.profile_name.clone()),
        vault: resolved.profile.settings.get("vault").map_or_else(
            bcode_config::default_auth_vault_path,
            std::path::PathBuf::from,
        ),
        map: resolved.profile.map.clone(),
        device_seal: resolved.profile.settings.get("device_seal").cloned(),
    }
}

fn metadata_for_primary(resolved: &crate::ResolvedAuthProfile) -> RuntimeAuthSubscriptionProfile {
    let value = metadata(resolved);
    RuntimeAuthSubscriptionProfile {
        auth_profile: resolved.profile_name.clone(),
        storage_profile: value.storage_profile,
        vault: value.vault,
        provider: value.provider_id,
        scheme: value.scheme,
        owner_plugin_id: Some(value.owner_plugin_id),
        map: value.map,
        device_seal: value.device_seal,
    }
}

/// Validate all pool/account decisions before OAuth. Capability must come from the enabled owner.
///
/// # Errors
/// Returns an error for ambiguous, incompatible, or unowned destinations without modifying state.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    provider: &AuthProviderContribution,
    owner_plugin_id: &str,
    method_id: &str,
    destination: EnrollmentDestination,
    pooling: PoolEnrollment,
    compatibility: Option<&AuthPoolCompatibility>,
) -> Result<EnrollmentPlan, EnrollmentError> {
    let prepared = prepare(
        config,
        runtime,
        provider,
        owner_plugin_id,
        method_id,
        destination,
    )?;
    let metadata = metadata(&prepared.resolved);
    let mut result = EnrollmentResult {
        profile: prepared.resolved.profile_name.clone(),
        pool: None,
        automatic: false,
        joined: false,
    };
    let context = config.active_context.is_some();
    if context && matches!(&pooling, PoolEnrollment::Explicit(_)) {
        return Err(conflict(
            None,
            Some(&result.profile),
            Source::Configuration,
            Reason::ContextBoundary,
        ));
    }
    let eligible = if let Some(capability) = compatibility {
        crate::managed_pool::validate_compatibility(provider, capability)?;
        capability.methods.iter().any(|method| method == method_id)
    } else {
        false
    };
    let automatic = matches!(pooling, PoolEnrollment::Automatic);
    let planner = PoolPlanner {
        config,
        runtime,
        provider,
        owner_plugin_id,
    };
    let pool_name = planner.select(pooling, context, eligible, compatibility)?;
    let pool = if let Some(name) = pool_name {
        let pool = planner.pool(&name, automatic, compatibility, &metadata, &mut result)?;
        result.pool = Some(name.clone());
        result.automatic = automatic;
        Some((name, pool))
    } else {
        None
    };
    if runtime
        .profiles
        .get(&result.profile)
        .is_some_and(|old| old != &metadata)
        || runtime
            .bindings
            .get(&provider.provider_id)
            .is_some_and(|binding| binding.owner_plugin_id != owner_plugin_id)
    {
        return Err(conflict(
            None,
            Some(&result.profile),
            Source::Combined,
            Reason::MemberMetadata,
        ));
    }
    let original = prepared.resolved.clone();
    Ok(EnrollmentPlan {
        preserve_memberships: false,
        original,
        prepared,
        expected: runtime.clone(),
        metadata,
        pool,
        result,
        context,
        authorization: authorization(config)?,
    })
}

struct PoolPlanner<'a> {
    config: &'a BcodeConfig,
    runtime: &'a RuntimeAuthSubscriptions,
    provider: &'a AuthProviderContribution,
    owner_plugin_id: &'a str,
}

impl PoolPlanner<'_> {
    fn select(
        &self,
        pooling: PoolEnrollment,
        context: bool,
        eligible: bool,
        compatibility: Option<&AuthPoolCompatibility>,
    ) -> Result<Option<String>, EnrollmentError> {
        let Self {
            config,
            runtime,
            provider,
            owner_plugin_id,
        } = *self;
        Ok(match pooling {
            PoolEnrollment::Standalone => None,
            PoolEnrollment::Automatic if context || !eligible => None,
            PoolEnrollment::Automatic => {
                let capability = compatibility.ok_or(EnrollmentError::InvalidMethod)?;
                let existing = crate::managed_pool::managed_pool(
                    config,
                    runtime,
                    &provider.provider_id,
                    owner_plugin_id,
                    &capability.group,
                )?;
                if let Some(name) = existing {
                    Some(name)
                } else {
                    let name = format!("{}-{}-default", provider.provider_id, capability.group);
                    if runtime.pools.contains_key(&name) || config.auth.pools.contains_key(&name) {
                        return Err(conflict(
                            Some(&name),
                            None,
                            Source::Combined,
                            Reason::NameCollision,
                        ));
                    }
                    Some(name)
                }
            }
            PoolEnrollment::Explicit(name) => {
                if name.trim().is_empty() || !eligible {
                    return Err(conflict(
                        Some(&name),
                        None,
                        Source::Combined,
                        Reason::MethodNotEligible,
                    ));
                }
                Some(name)
            }
        })
    }
    fn pool(
        &self,
        name: &str,
        automatic: bool,
        compatibility: Option<&AuthPoolCompatibility>,
        metadata: &RuntimeAuthProfile,
        result: &mut EnrollmentResult,
    ) -> Result<RuntimeAuthSubscriptionPool, EnrollmentError> {
        let Self {
            config,
            runtime,
            provider,
            owner_plugin_id,
        } = *self;
        let declared_marker = config
            .auth
            .pools
            .get(name)
            .and_then(|pool| pool.automatic_group.as_deref());
        let runtime_marker = runtime
            .pools
            .get(name)
            .and_then(|pool| pool.automatic_group.as_deref());
        if [declared_marker, runtime_marker]
            .into_iter()
            .flatten()
            .any(|marker| Some(marker) != compatibility.map(|capability| capability.group.as_str()))
        {
            return Err(conflict(
                Some(name),
                None,
                Source::Combined,
                Reason::CompatibilityGroup,
            ));
        }
        let group = if automatic || declared_marker.is_some() || runtime_marker.is_some() {
            compatibility.map(|capability| capability.group.as_str())
        } else {
            None
        };
        if let Some(declared) = config.auth.pools.get(name)
            && (declared.provider_plugin_id.as_deref() != Some(owner_plugin_id)
                || (declared.automatic_group.is_some()
                    && declared.automatic_provider.as_deref()
                        != Some(provider.provider_id.as_str()))
                || (automatic && declared.automatic_group.as_deref() != group))
        {
            return Err(conflict(
                Some(name),
                None,
                Source::Configuration,
                Reason::Ownership,
            ));
        }
        let mut pool = if let Some(existing) = runtime.pools.get(name) {
            let declared_opt_in = config.auth.pools.get(name).is_some_and(|pool| {
                pool.automatic_provider.as_deref() == Some(provider.provider_id.as_str())
                    && pool.automatic_group.as_deref() == group
                    && pool.provider_plugin_id.as_deref() == Some(owner_plugin_id)
            });
            crate::managed_pool::validate_pool(
                existing,
                &provider.provider_id,
                owner_plugin_id,
                if declared_opt_in && existing.automatic_group.is_none() {
                    None
                } else {
                    group
                },
            )
            .map_err(|error| error.in_pool(name))?;
            let mut existing = existing.clone();
            if declared_opt_in {
                existing.automatic_group = group.map(str::to_owned);
            }
            existing
        } else {
            RuntimeAuthSubscriptionPool {
                provider_plugin_id: Some(owner_plugin_id.to_owned()),
                provider_id: Some(provider.provider_id.clone()),
                owner_plugin_id: Some(owner_plugin_id.to_owned()),
                automatic_group: group.map(str::to_owned),
                ..RuntimeAuthSubscriptionPool::default()
            }
        };
        self.validate_members(name, compatibility.ok_or(EnrollmentError::PoolConflict)?)?;
        if automatic {
            self.include_primary(
                &mut pool,
                &result.profile,
                compatibility.ok_or(EnrollmentError::PoolConflict)?,
            )?;
        }
        self.include_target(name, &mut pool, metadata, result)?;
        Ok(pool)
    }
    fn validate_members(
        &self,
        name: &str,
        capability: &AuthPoolCompatibility,
    ) -> Result<(), EnrollmentError> {
        let Self {
            config,
            runtime,
            provider,
            owner_plugin_id,
        } = *self;
        // Validate every effective member before permitting publication, not merely the new account.
        for member in bcode_config::effective_auth_pool_order(config, runtime, name, None).profiles
        {
            let existing = crate::resolve_auth_provider_profile(
                config,
                &provider.provider_id,
                owner_plugin_id,
                Some(&member),
                runtime,
            )?;
            if !capability
                .methods
                .iter()
                .any(|method| Some(method.as_str()) == existing.profile.scheme.as_deref())
            {
                return Err(conflict(
                    Some(name),
                    Some(&member),
                    Source::Combined,
                    Reason::MemberMethod,
                ));
            }
        }
        Ok(())
    }
    fn include_primary(
        &self,
        pool: &mut RuntimeAuthSubscriptionPool,
        target: &str,
        capability: &AuthPoolCompatibility,
    ) -> Result<(), EnrollmentError> {
        let Self {
            config,
            runtime,
            provider,
            owner_plugin_id,
        } = *self;
        // Only the canonical provider binding/default may join alongside the target.
        let primary = crate::resolve_auth_provider_profile(
            config,
            &provider.provider_id,
            owner_plugin_id,
            None,
            runtime,
        );
        match primary {
            Ok(primary)
                if primary.profile_name != target
                    && capability.methods.iter().any(|method| {
                        Some(method.as_str()) == primary.profile.scheme.as_deref()
                    }) =>
            {
                if primary.profile.backend != "sshenv" {
                    return Err(conflict(
                        None,
                        Some(&primary.profile_name),
                        Source::Combined,
                        Reason::MemberMetadata,
                    ));
                }
                let primary_metadata = metadata_for_primary(&primary);
                if let Some(existing) = pool
                    .profiles
                    .iter()
                    .find(|member| member.auth_profile == primary.profile_name)
                {
                    if existing != &primary_metadata {
                        return Err(conflict(
                            None,
                            Some(&primary.profile_name),
                            Source::Combined,
                            Reason::MemberMetadata,
                        ));
                    }
                } else {
                    pool.profiles.push(primary_metadata);
                }
            }
            Ok(_) => {}
            Err(crate::AuthProfileResolutionError::MissingProfile { .. })
                if !config.auth.bindings.contains_key(&provider.provider_id)
                    && !config.auth.profiles.contains_key(&provider.provider_id)
                    && !runtime.bindings.contains_key(&provider.provider_id) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    fn include_target(
        &self,
        name: &str,
        pool: &mut RuntimeAuthSubscriptionPool,
        metadata: &RuntimeAuthProfile,
        result: &mut EnrollmentResult,
    ) -> Result<(), EnrollmentError> {
        let config = self.config;
        let mut seen = std::collections::BTreeSet::new();
        if pool
            .profiles
            .iter()
            .any(|member| !seen.insert(&member.auth_profile))
        {
            return Err(conflict(
                Some(name),
                None,
                Source::Runtime,
                Reason::MemberIdentity,
            ));
        }
        let member = RuntimeAuthSubscriptionProfile {
            auth_profile: result.profile.clone(),
            storage_profile: metadata.storage_profile.clone(),
            vault: metadata.vault.clone(),
            provider: metadata.provider_id.clone(),
            scheme: metadata.scheme.clone(),
            owner_plugin_id: Some(metadata.owner_plugin_id.clone()),
            map: metadata.map.clone(),
            device_seal: metadata.device_seal.clone(),
        };
        if let Some(existing) = pool
            .profiles
            .iter()
            .find(|existing| existing.auth_profile == result.profile)
        {
            if existing != &member {
                return Err(conflict(
                    Some(name),
                    Some(&result.profile),
                    Source::Combined,
                    Reason::MemberMetadata,
                ));
            }
        } else {
            result.joined = !config
                .auth
                .pools
                .get(name)
                .is_some_and(|declared| declared.profiles.contains(&result.profile));
            pool.profiles.push(member);
        }
        Ok(())
    }
}

/// Atomically publish non-secret metadata and membership only AFTER credentials are saved.
/// Existing standalone accounts can use this after a fresh plan without redoing OAuth.
///
/// # Errors
/// Returns an error if registry changed since planning, ownership conflicts, or persistence fails.
fn authorization(config: &BcodeConfig) -> Result<serde_json::Value, EnrollmentError> {
    serde_json::to_value((&config.auth, &config.active_context))
        .map_err(|_| EnrollmentError::PoolConflict)
}

/// Validate the plan against current configuration without filesystem or publication effects.
///
/// # Errors
/// Returns an error for configuration drift or post-plan identity/destination/policy changes.
pub fn complete_with_config(
    plan: &EnrollmentPlan,
    current_config: &BcodeConfig,
) -> Result<EnrollmentResult, bcode_config::ConfigError> {
    validate_completion(plan, current_config)?;
    Ok(plan.result.clone())
}

fn validate_completion(
    plan: &EnrollmentPlan,
    current_config: &BcodeConfig,
) -> Result<(), bcode_config::ConfigError> {
    if authorization(current_config).ok().as_ref() != Some(&plan.authorization) {
        return Err(bcode_config::ConfigError::Composition { message: "Authentication configuration changed during enrollment; credentials saved but metadata not published. Replan before attaching.".to_owned() });
    }
    let mut current = plan.prepared.resolved.clone();
    let device_seal = current.profile.settings.get("device_seal").cloned();
    if device_seal.as_deref() != Some("disabled")
        && device_seal != plan.original.profile.settings.get("device_seal").cloned()
    {
        return Err(bcode_config::ConfigError::Composition {
            message: "Enrollment policy changed after planning".to_owned(),
        });
    }
    current.profile.settings.remove("device_seal");
    let mut original = plan.original.clone();
    original.profile.settings.remove("device_seal");
    if current != original {
        return Err(bcode_config::ConfigError::Composition {
            message: "Enrollment destination changed after planning".to_owned(),
        });
    }
    Ok(())
}

/// Reload effective configuration, validate authorization, then atomically publish after custody.
///
/// # Errors
/// Returns an error for configuration/registry drift, changed policy or failed persistence.
pub fn complete(plan: &EnrollmentPlan) -> Result<EnrollmentResult, bcode_config::ConfigError> {
    let config = bcode_config::load_config()?;
    validate_completion(plan, &config)?;
    if plan.preserve_memberships {
        let runtime = bcode_config::try_load_runtime_auth_subscriptions()?;
        let current = crate::resolve_auth_provider_profile(
            &config, &plan.original.provider_id, &plan.original.owner_plugin_id,
            Some(&plan.original.profile_name), &runtime,
        ).map_err(|_| bcode_config::ConfigError::Composition { message: "Account ownership changed during reauthentication; credentials saved but routing metadata was not changed.".to_owned() })?;
        if current != plan.original || plan.prepared.resolved != plan.original {
            return Err(bcode_config::ConfigError::Composition { message: "Account destination or policy changed during reauthentication; routing metadata preserved. Inspect account state before retrying.".to_owned() });
        }
        return Ok(plan.result.clone());
    }
    let device_seal = plan
        .prepared
        .resolved
        .profile
        .settings
        .get("device_seal")
        .cloned();
    let mut metadata = plan.metadata.clone();
    metadata.device_seal = device_seal;
    let mut pool = plan.pool.clone();
    if let Some((_, pool)) = &mut pool {
        for member in &mut pool.profiles {
            if member.auth_profile == plan.result.profile {
                member.device_seal.clone_from(&metadata.device_seal);
            }
        }
    }
    if !plan.context {
        bcode_config::publish_auth_enrollment(
            &plan.expected,
            &plan.result.profile,
            &metadata,
            pool.as_ref().map(|(name, pool)| (name.as_str(), pool)),
        )?;
    }
    Ok(plan.result.clone())
}
