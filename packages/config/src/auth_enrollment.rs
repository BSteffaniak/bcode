//! Atomic non-secret enrollment publication.

#[cfg(test)]
mod tests;

use super::{
    ConfigError, RuntimeAuthBinding, RuntimeAuthProfile, RuntimeAuthSubscriptionPool,
    RuntimeAuthSubscriptions,
};

/// Publish a validated enrollment against its exact preparation snapshot.
/// Credentials must already have been saved successfully by the caller.
///
/// # Errors
/// Returns an error without replacing registry state for concurrent edits, unreadable or
/// unsupported state, conflicting profiles/bindings, or failed atomic persistence.
pub fn publish_auth_enrollment(
    expected: &RuntimeAuthSubscriptions,
    name: &str,
    profile: &RuntimeAuthProfile,
    pool: Option<(&str, &RuntimeAuthSubscriptionPool)>,
) -> Result<(), ConfigError> {
    publish_auth_enrollment_at(
        &super::runtime_auth_subscriptions_path(),
        expected,
        name,
        profile,
        pool,
    )
}

fn publish_auth_enrollment_at(
    path: &std::path::Path,
    expected: &RuntimeAuthSubscriptions,
    name: &str,
    profile: &RuntimeAuthProfile,
    pool: Option<(&str, &RuntimeAuthSubscriptionPool)>,
) -> Result<(), ConfigError> {
    let _lock = super::lock_auth_subscriptions(path)?;
    let mut registry = super::read_auth_subscriptions_for_update(path)?;
    if &registry != expected {
        return Err(ConfigError::Composition { message: "Authentication registry changed during enrollment; credentials were saved but metadata was not published. Retry enrollment planning.".to_owned() });
    }
    if name.trim().is_empty()
        || profile.provider_id.is_empty()
        || profile.owner_plugin_id.is_empty()
        || registry.profiles.get(name).is_some_and(|old| {
            let mut expected = old.clone();
            expected.device_seal.clone_from(&profile.device_seal);
            expected != *profile
        })
        || registry
            .bindings
            .get(&profile.provider_id)
            .is_some_and(|old| old.owner_plugin_id != profile.owner_plugin_id)
    {
        return Err(ConfigError::Composition {
            message: "Authentication enrollment ownership conflicts; metadata preserved".to_owned(),
        });
    }
    registry.profiles.insert(name.to_owned(), profile.clone());
    registry
        .bindings
        .entry(profile.provider_id.clone())
        .or_insert_with(|| RuntimeAuthBinding {
            profile: name.to_owned(),
            owner_plugin_id: profile.owner_plugin_id.clone(),
        });
    if let Some((name, pool)) = pool {
        if name.trim().is_empty()
            || pool.provider_id.as_deref() != Some(profile.provider_id.as_str())
            || pool.owner_plugin_id.as_deref() != Some(profile.owner_plugin_id.as_str())
            || pool.provider_plugin_id.as_deref() != Some(profile.owner_plugin_id.as_str())
        {
            return Err(ConfigError::Composition {
                message: "Authentication pool ownership conflicts; metadata preserved".to_owned(),
            });
        }
        registry.pools.insert(name.to_owned(), pool.clone());
    }
    super::write_runtime_auth_subscriptions(path, &registry)?;
    Ok(())
}
