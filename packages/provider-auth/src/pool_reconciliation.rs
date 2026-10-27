//! Explicit managed-pool reconciliation; never reads or rewrites credentials.

use bcode_config::{
    BcodeConfig, ConfigError, RuntimeAuthSubscriptionPool, RuntimeAuthSubscriptions,
};

fn invalid(message: &str) -> ConfigError {
    ConfigError::Composition {
        message: message.to_owned(),
    }
}

/// A reviewed reconciliation bound to exact configuration and registry snapshots.
pub struct PoolReconciliation {
    config: BcodeConfig,
    registry: RuntimeAuthSubscriptions,
    source: String,
    destination: String,
    replacement: RuntimeAuthSubscriptionPool,
}

impl PoolReconciliation {
    /// Names whose existing credential destinations are preserved.
    #[must_use]
    pub fn profiles(&self) -> Vec<&str> {
        self.replacement
            .profiles
            .iter()
            .map(|profile| profile.auth_profile.as_str())
            .collect()
    }

    /// Apply only after explicit authorization. Source observations remain for interruption recovery.
    ///
    /// # Errors
    /// Rejects changed configuration/registry, incompatible observations or persistence failures.
    pub fn apply(&self) -> Result<(), ConfigError> {
        if bcode_config::load_config()? != self.config {
            return Err(invalid(
                "Configuration changed; preview reconciliation again",
            ));
        }
        bcode_config::publish_auth_pool_reconciliation(
            &self.registry,
            &self.source,
            &self.destination,
            &self.replacement,
            || crate::auth_pool_state::preserve_pool_observations(&self.source, &self.destination),
        )
    }
}

/// Plan moving a runtime automatic pool into a declared automatic pool of the same owner/group.
/// Rejects ambiguity rather than changing credentials or guessing preference conflicts.
///
/// # Errors
/// Returns an error for incompatible ownership, member destinations, preferences or config scope.
pub fn plan_pool_reconciliation(
    config: &BcodeConfig,
    registry: &RuntimeAuthSubscriptions,
    source: &str,
    destination: &str,
) -> Result<PoolReconciliation, ConfigError> {
    let reject =
        || invalid("Pool reconciliation is ambiguous or incompatible; existing state preserved");
    if config.active_context.is_some()
        || config.model.auth_pool.as_deref() == Some(source)
        || config
            .model
            .profiles
            .values()
            .any(|profile| profile.auth_pool.as_deref() == Some(source))
        || source == destination
        || config.auth.pools.contains_key(source)
    {
        return Err(reject());
    }
    let declared = config.auth.pools.get(destination).ok_or_else(reject)?;
    let provider = declared.automatic_provider.as_deref().ok_or_else(reject)?;
    let owner = declared.provider_plugin_id.as_deref().ok_or_else(reject)?;
    let group = declared.automatic_group.as_deref().ok_or_else(reject)?;
    let original = registry.pools.get(source).ok_or_else(reject)?;
    crate::managed_pool::validate_pool(original, provider, owner, Some(group))
        .map_err(|_| reject())?;
    let mut replacement = original.clone();
    if let Some(existing) = registry.pools.get(destination) {
        crate::managed_pool::validate_pool(
            existing,
            provider,
            owner,
            existing.automatic_group.as_deref(),
        )
        .map_err(|_| reject())?;
        if existing
            .automatic_group
            .as_deref()
            .is_some_and(|value| value != group)
        {
            return Err(reject());
        }
        replacement = existing.clone();
        replacement.automatic_group = Some(group.to_owned());
        for member in &original.profiles {
            if let Some(found) = replacement
                .profiles
                .iter()
                .find(|entry| entry.auth_profile == member.auth_profile)
            {
                if found != member {
                    return Err(reject());
                }
            } else {
                replacement.profiles.push(member.clone());
            }
        }
        match (&existing.preferred_profile, &original.preferred_profile) {
            (Some(left), Some(right)) if left != right => return Err(reject()),
            (None, preference) => replacement.preferred_profile.clone_from(preference),
            _ => {}
        }
    }
    for member in &replacement.profiles {
        let resolved = crate::resolve_auth_provider_profile(
            config,
            provider,
            owner,
            Some(&member.auth_profile),
            registry,
        )
        .map_err(|_| reject())?;
        if resolved.profile.settings.get("profile") != Some(&member.storage_profile)
            || resolved
                .profile
                .settings
                .get("vault")
                .map(std::path::Path::new)
                != Some(member.vault.as_path())
            || resolved.profile.scheme.as_deref() != Some(member.scheme.as_str())
            || resolved.profile.backend != "sshenv"
            || resolved.profile.settings.get("device_seal") != member.device_seal.as_ref()
            || (!member.map.is_empty() && resolved.profile.map != member.map)
        {
            return Err(reject());
        }
    }
    if replacement
        .preferred_profile
        .as_ref()
        .is_some_and(|preferred| {
            !replacement
                .profiles
                .iter()
                .any(|member| &member.auth_profile == preferred)
        })
    {
        return Err(reject());
    }
    let mut reconciled = registry.clone();
    reconciled.pools.remove(source);
    reconciled
        .pools
        .insert(destination.to_owned(), replacement.clone());
    if bcode_config::effective_auth_pool_order(config, &reconciled, destination, None)
        .degraded_reason
        .is_some()
    {
        return Err(reject());
    }
    Ok(PoolReconciliation {
        config: config.clone(),
        registry: registry.clone(),
        source: source.to_owned(),
        destination: destination.to_owned(),
        replacement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_config::{AuthPoolConfig, RuntimeAuthProfile, RuntimeAuthSubscriptionProfile};

    fn fixture() -> (BcodeConfig, RuntimeAuthSubscriptions) {
        let mut config = BcodeConfig::default();
        config.auth.pools.insert(
            "chosen".into(),
            AuthPoolConfig {
                automatic_provider: Some("example".into()),
                automatic_group: Some("subscription".into()),
                provider_plugin_id: Some("example.plugin".into()),
                ..Default::default()
            },
        );
        let mut registry = RuntimeAuthSubscriptions::default();
        let members: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|name| {
                registry.profiles.insert(
                    name.into(),
                    RuntimeAuthProfile {
                        provider_id: "example".into(),
                        owner_plugin_id: "example.plugin".into(),
                        backend: "sshenv".into(),
                        scheme: "chatgpt".into(),
                        storage_profile: name.into(),
                        vault: "/synthetic/vault".into(),
                        ..Default::default()
                    },
                );
                RuntimeAuthSubscriptionProfile {
                    auth_profile: name.into(),
                    storage_profile: name.into(),
                    vault: "/synthetic/vault".into(),
                    provider: "example".into(),
                    scheme: "chatgpt".into(),
                    owner_plugin_id: Some("example.plugin".into()),
                    ..Default::default()
                }
            })
            .collect();
        registry.pools.insert(
            "old".into(),
            RuntimeAuthSubscriptionPool {
                automatic_group: Some("subscription".into()),
                provider_id: Some("example".into()),
                owner_plugin_id: Some("example.plugin".into()),
                provider_plugin_id: Some("example.plugin".into()),
                profiles: members,
                preferred_profile: Some("two".into()),
            },
        );
        (config, registry)
    }

    #[test]
    fn reconciliation_preserves_members_and_preference_without_materializing_secrets() {
        let (config, registry) = fixture();
        let plan = plan_pool_reconciliation(&config, &registry, "old", "chosen").unwrap();
        assert_eq!(plan.profiles(), ["one", "two"]);
        assert_eq!(plan.replacement, registry.pools["old"]);
        assert!(
            bcode_config::effective_auth_pool_order(&config, &registry, "chosen", Some("one"))
                .degraded_reason
                .is_some()
        );
        let request = crate::ProviderRequestContextResolution {
            config: &config,
            selection: bcode_config::ResolvedModelSelection {
                auth_pool: Some("chosen".into()),
                ..Default::default()
            },
        };
        assert!(crate::validate_auth_selection_metadata(&request, &registry).is_err());
        let mut reconciled = registry.clone();
        reconciled.pools.remove("old");
        reconciled.pools.insert("chosen".into(), plan.replacement);
        assert!(
            bcode_config::effective_auth_pool_order(&config, &reconciled, "chosen", None)
                .degraded_reason
                .is_none()
        );
        assert_eq!(reconciled.profiles, registry.profiles);
    }

    #[test]
    fn reconciliation_rejects_foreign_ownership_destination_and_preference_conflicts() {
        let (config, registry) = fixture();
        let mut foreign = registry.clone();
        foreign.pools.get_mut("old").unwrap().owner_plugin_id = Some("foreign".into());
        assert!(plan_pool_reconciliation(&config, &foreign, "old", "chosen").is_err());
        let mut conflict = registry.clone();
        let mut other = registry.pools["old"].clone();
        other.preferred_profile = Some("one".into());
        conflict.pools.insert("chosen".into(), other);
        assert!(plan_pool_reconciliation(&config, &conflict, "old", "chosen").is_err());
        let mut changed = registry.clone();
        changed.profiles.get_mut("one").unwrap().vault = "/different".into();
        assert!(plan_pool_reconciliation(&config, &changed, "old", "chosen").is_err());
        assert_eq!(registry.pools.len(), 1);
    }
}
