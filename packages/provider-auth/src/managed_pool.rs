//! Shared automatic/default pool planning. No secrets or ambient filesystem reads.

use crate::{ResolvedAuthProfile, enrollment::EnrollmentError};
use bcode_config::{BcodeConfig, RuntimeAuthSubscriptionPool, RuntimeAuthSubscriptions};
use bcode_provider_auth_models::{
    AuthMethodContribution, AuthPoolCompatibility, AuthProviderContribution,
};

/// Original request intent, before any pool selection or credential materialization.
pub struct ManagedDefaultSelection<'a> {
    /// Selected model-provider plugin.
    pub provider_plugin_id: &'a str,
    /// Original explicit account; never a routed account.
    pub auth_profile: Option<&'a str>,
    /// Original explicit pool.
    pub auth_pool: Option<&'a str>,
    /// True only for daemon-owned native default resolution, not externally supplied snapshots.
    pub native_defaults: bool,
}

/// In-process native-default authority. Deliberately not serializable or constructible by clients.
#[derive(Debug, Clone)]
pub struct ManagedDefaultRoute {
    provider: AuthProviderContribution,
    owner: String,
    compatibility: AuthPoolCompatibility,
    auth: serde_json::Value,
    selected_pool: Option<String>,
    primary: Option<String>,
}

/// Authoritative membership names; existing credential material must not be refreshed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedDefaultMembership {
    /// Managed pool name.
    pub pool: String,
    /// Members in effective preference order.
    pub profiles: Vec<String>,
}

pub(crate) fn validate_compatibility(
    provider: &AuthProviderContribution,
    compatibility: &AuthPoolCompatibility,
) -> Result<(), EnrollmentError> {
    provider
        .validate()
        .map_err(|_| EnrollmentError::InvalidMethod)?;
    compatibility
        .validate()
        .map_err(|_| EnrollmentError::InvalidMethod)?;
    if compatibility.provider_id != provider.provider_id || compatibility.methods.iter().any(|id| {
        !provider.methods.iter().any(|method| matches!(method, AuthMethodContribution::Interactive { method_id, .. } if method_id == id))
    }) {
        return Err(EnrollmentError::InvalidMethod);
    }
    Ok(())
}

/// Mint native-default provenance, including before the first managed pool exists.
///
/// # Errors
/// Returns an error for invalid plugin capability or provider ownership.
pub fn mint_managed_default(
    config: &BcodeConfig,
    selection: &ManagedDefaultSelection<'_>,
    provider: &AuthProviderContribution,
    owner_plugin_id: &str,
    compatibility: &AuthPoolCompatibility,
) -> Result<Option<ManagedDefaultRoute>, EnrollmentError> {
    mint_managed_default_with_runtime(
        config,
        &RuntimeAuthSubscriptions::default(),
        selection,
        provider,
        owner_plugin_id,
        compatibility,
    )
}

/// Mint provenance using a trustworthy registry for explicit runtime-only managed pools.
///
/// # Errors
/// Returns an error for incompatible markers, capability or ownership.
pub fn mint_managed_default_with_runtime(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    selection: &ManagedDefaultSelection<'_>,
    provider: &AuthProviderContribution,
    owner_plugin_id: &str,
    compatibility: &AuthPoolCompatibility,
) -> Result<Option<ManagedDefaultRoute>, EnrollmentError> {
    if config.active_context.is_some()
        || (selection.auth_profile.is_some() && selection.auth_pool.is_none())
        || !selection.native_defaults
    {
        return Ok(None);
    }
    validate_compatibility(provider, compatibility)?;
    if owner_plugin_id.is_empty() || selection.provider_plugin_id != owner_plugin_id {
        return Err(EnrollmentError::PoolConflict);
    }
    if let Some(name) = selection.auth_pool {
        if let Some(pool) = config.auth.pools.get(name) {
            if pool.automatic_group.is_none() {
                return Ok(None);
            }
            validate_declared(
                pool,
                &provider.provider_id,
                owner_plugin_id,
                &compatibility.group,
            )?;
        } else if let Some(pool) = runtime.pools.get(name) {
            if pool.automatic_group.is_none() {
                return Ok(None);
            }
            validate_pool(
                pool,
                &provider.provider_id,
                owner_plugin_id,
                Some(&compatibility.group),
            )?;
        } else {
            return Ok(None);
        }
    }
    Ok(Some(ManagedDefaultRoute {
        provider: provider.clone(),
        owner: owner_plugin_id.to_owned(),
        compatibility: compatibility.clone(),
        selected_pool: selection.auth_pool.map(str::to_owned),
        primary: selection.auth_profile.map(str::to_owned),
        auth: serde_json::to_value(&config.auth).map_err(|_| EnrollmentError::PoolConflict)?,
    }))
}

pub(crate) fn validate_declared(
    pool: &bcode_config::AuthPoolConfig,
    provider: &str,
    owner: &str,
    group: &str,
) -> Result<(), EnrollmentError> {
    if pool.automatic_provider.as_deref() != Some(provider)
        || pool.provider_plugin_id.as_deref() != Some(owner)
        || pool.automatic_group.as_deref() != Some(group)
    {
        return Err(EnrollmentError::PoolConflict);
    }
    Ok(())
}

pub(crate) fn managed_pool(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    provider: &str,
    owner: &str,
    group: &str,
) -> Result<Option<String>, EnrollmentError> {
    let mut names = std::collections::BTreeSet::new();
    for (name, pool) in &config.auth.pools {
        if pool.automatic_group.as_deref() == Some(group)
            && pool.provider_plugin_id.as_deref() == Some(owner)
        {
            if pool.automatic_provider.is_none() {
                return Err(EnrollmentError::PoolConflict);
            }
            if pool.automatic_provider.as_deref() == Some(provider) {
                names.insert(name.clone());
            }
        }
    }
    for (name, pool) in &runtime.pools {
        if pool.automatic_group.as_deref() == Some(group)
            && pool.provider_id.as_deref() == Some(provider)
            && (pool.owner_plugin_id.as_deref() == Some(owner)
                || pool.provider_plugin_id.as_deref() == Some(owner))
        {
            validate_pool(pool, provider, owner, Some(group))?;
            names.insert(name.clone());
        }
    }
    if names.len() > 1 {
        return Err(EnrollmentError::PoolConflict);
    }
    let name = names.into_iter().next();
    if let Some(name) = &name {
        if let Some(pool) = runtime.pools.get(name) {
            let declared_opt_in = config.auth.pools.get(name).is_some_and(|pool| {
                pool.automatic_provider.as_deref() == Some(provider)
                    && pool.automatic_group.as_deref() == Some(group)
                    && pool.provider_plugin_id.as_deref() == Some(owner)
            });
            validate_pool(
                pool,
                provider,
                owner,
                if declared_opt_in && pool.automatic_group.is_none() {
                    None
                } else {
                    Some(group)
                },
            )?;
        }
        if let Some(pool) = config.auth.pools.get(name) {
            validate_declared(pool, provider, owner, group)?;
        }
    }
    Ok(name)
}

pub(crate) fn validate_pool(
    pool: &RuntimeAuthSubscriptionPool,
    provider: &str,
    owner: &str,
    group: Option<&str>,
) -> Result<(), EnrollmentError> {
    if pool.owner_plugin_id.as_deref() != Some(owner)
        || pool.provider_plugin_id.as_deref() != Some(owner)
        || pool.provider_id.as_deref() != Some(provider)
        || (group.is_some() && pool.automatic_group.as_deref() != group)
    {
        return Err(EnrollmentError::PoolConflict);
    }
    let mut members = std::collections::BTreeSet::new();
    if pool.profiles.iter().any(|member| {
        member.auth_profile.trim().is_empty()
            || !members.insert(&member.auth_profile)
            || member.provider != provider
            || member.owner_plugin_id.as_deref() != Some(owner)
    }) {
        return Err(EnrollmentError::PoolConflict);
    }
    Ok(())
}

/// Re-read names from a supplied trustworthy current registry; never reads credentials.
///
/// # Errors
/// Returns an error on context drift, ownership conflicts, ambiguity or incompatible members.
pub fn refresh_managed_default(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    receipt: &ManagedDefaultRoute,
) -> Result<Option<ManagedDefaultMembership>, EnrollmentError> {
    if config.active_context.is_some()
        || serde_json::to_value(&config.auth).map_err(|_| EnrollmentError::PoolConflict)?
            != receipt.auth
    {
        return Err(EnrollmentError::PoolConflict);
    }
    let selected = if let Some(name) = &receipt.selected_pool {
        if let Some(declared) = config.auth.pools.get(name) {
            validate_declared(
                declared,
                &receipt.provider.provider_id,
                &receipt.owner,
                &receipt.compatibility.group,
            )?;
        } else if !runtime.pools.contains_key(name) {
            return Err(EnrollmentError::PoolConflict);
        }
        if let Some(pool) = runtime.pools.get(name) {
            validate_pool(
                pool,
                &receipt.provider.provider_id,
                &receipt.owner,
                if pool.automatic_group.is_none() && config.auth.pools.contains_key(name) {
                    None
                } else {
                    Some(&receipt.compatibility.group)
                },
            )?;
        }
        Some(name.clone())
    } else {
        managed_pool(
            config,
            runtime,
            &receipt.provider.provider_id,
            &receipt.owner,
            &receipt.compatibility.group,
        )?
    };
    let Some(pool) = selected else {
        return Ok(None);
    };
    let profiles =
        bcode_config::effective_auth_pool_order(config, runtime, &pool, receipt.primary.as_deref())
            .profiles;
    for name in &profiles {
        resolve_member(config, runtime, receipt, name)?;
    }
    Ok(Some(ManagedDefaultMembership { pool, profiles }))
}

fn resolve_member(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    receipt: &ManagedDefaultRoute,
    name: &str,
) -> Result<ResolvedAuthProfile, EnrollmentError> {
    let resolved = crate::resolve_auth_provider_profile(
        config,
        &receipt.provider.provider_id,
        &receipt.owner,
        Some(name),
        runtime,
    )?;
    if !receipt
        .compatibility
        .methods
        .iter()
        .any(|method| Some(method.as_str()) == resolved.profile.scheme.as_deref())
    {
        return Err(EnrollmentError::PoolConflict);
    }
    Ok(resolved)
}

/// Ownership-checked blueprint for exactly one newly discovered member, not secret material.
///
/// # Errors
/// Returns an error if the receipt is no longer valid or the name is not an eligible member.
pub fn resolve_managed_member(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    receipt: &ManagedDefaultRoute,
    name: &str,
) -> Result<ResolvedAuthProfile, EnrollmentError> {
    let membership =
        refresh_managed_default(config, runtime, receipt)?.ok_or(EnrollmentError::PoolConflict)?;
    if !membership.profiles.iter().any(|member| member == name) {
        return Err(EnrollmentError::PoolConflict);
    }
    resolve_member(config, runtime, receipt, name)
}
