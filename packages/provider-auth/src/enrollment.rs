//! Renderer-independent preparation and publication of owned enrollment profiles.
//!
//! Preparation is read-only. Credential custody and interactive provider effects
//! are separate operations; callers publish metadata only after successful enrollment.

use crate::{AuthProfileResolutionError, AuthProfileSource, ResolvedAuthProfile};
use bcode_config::{
    AuthCredentialMapping, AuthProfileConfig, BcodeConfig, RuntimeAuthSubscriptions,
};
use bcode_provider_auth_models::{AuthMethodContribution, AuthProviderContribution};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Explicit enrollment destination, resolved after frontend review.
#[derive(Debug, Clone, Default)]
pub struct EnrollmentDestination {
    /// Profile already selected through the shared authentication resolution path.
    pub profile: Option<String>,
    /// Explicit vault override.
    pub vault: Option<PathBuf>,
    /// Optional SSH recipient public key.
    pub recipient_key: Option<String>,
}

/// Prepared profile and whether successful enrollment requires publishing metadata.
pub struct PreparedEnrollment {
    /// Ownership-checked profile, containing only credential references.
    pub resolved: ResolvedAuthProfile,
    /// Whether this operation created a new profile blueprint.
    pub publish_runtime: bool,
}

/// Enrollment preparation failure. No secret values are included.
#[derive(Debug, thiserror::Error)]
pub enum EnrollmentError {
    /// Registration or requested method is inconsistent.
    #[error("Authentication registration or method is invalid")]
    InvalidMethod,
    /// New context accounts require a declared destination until context registration is available.
    #[error(
        "Declare this context's authentication profile before enrollment; unscoped account creation is not permitted"
    )]
    ContextProfileRequired,
    /// Reusing a profile cannot silently change its credential destination.
    #[error(
        "Existing account uses a different vault; choose a new profile or explicitly migrate the account"
    )]
    DestinationConflict,
    /// Existing profile ownership cannot be established.
    #[error(transparent)]
    Profile(#[from] AuthProfileResolutionError),
}

/// Prepare an owned profile without creating files, reading secrets, or prompting.
///
/// # Errors
/// Returns an error for invalid registration/method or incompatible existing profile ownership.
pub fn prepare(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    provider: &AuthProviderContribution,
    owner_plugin_id: &str,
    method_id: &str,
    destination: EnrollmentDestination,
) -> Result<PreparedEnrollment, EnrollmentError> {
    provider
        .validate()
        .map_err(|_| EnrollmentError::InvalidMethod)?;
    let method = provider
        .methods
        .iter()
        .find(|method| method.method_id() == method_id)
        .ok_or(EnrollmentError::InvalidMethod)?;
    let EnrollmentDestination {
        profile,
        vault,
        recipient_key,
    } = destination;
    match crate::resolve_auth_provider_profile(
        config,
        &provider.provider_id,
        owner_plugin_id,
        profile.as_deref(),
        runtime,
    ) {
        Ok(mut resolved) => {
            crate::lifecycle::AuthVaultLifecycle::new(
                &resolved,
                &provider.provider_id,
                owner_plugin_id,
                method,
            )
            .map_err(|_| EnrollmentError::InvalidMethod)?;
            if let Some(vault) = vault {
                let existing_vault = resolved
                    .profile
                    .settings
                    .get("vault")
                    .map_or_else(bcode_config::default_auth_vault_path, PathBuf::from);
                if vault != existing_vault {
                    return Err(EnrollmentError::DestinationConflict);
                }
                resolved
                    .profile
                    .settings
                    .insert("vault".to_owned(), vault.display().to_string());
            }
            if let Some(recipient) = recipient_key {
                resolved
                    .profile
                    .settings
                    .insert("recipient_key".to_owned(), recipient);
            }
            Ok(PreparedEnrollment {
                resolved,
                publish_runtime: false,
            })
        }
        Err(AuthProfileResolutionError::MissingProfile { .. }) => {
            if config.active_context.is_some() {
                return Err(EnrollmentError::ContextProfileRequired);
            }
            let name = profile.unwrap_or_else(|| provider.provider_id.clone());
            validate_enrollment_binding(runtime, &provider.provider_id, owner_plugin_id, &name)?;
            let vault = vault.unwrap_or_else(bcode_config::default_auth_vault_path);
            let mut settings = BTreeMap::from([
                ("profile".to_owned(), name.clone()),
                ("vault".to_owned(), vault.display().to_string()),
            ]);
            if let Some(recipient) = recipient_key {
                settings.insert("recipient_key".to_owned(), recipient);
            }
            let map = match method {
                AuthMethodContribution::SecretFields { fields, .. } => fields
                    .iter()
                    .map(|field| (field.credential_id.clone(), mapping(&field.storage_key)))
                    .collect(),
                AuthMethodContribution::Interactive { credentials, .. } => credentials
                    .iter()
                    .map(|field| (field.credential_id.clone(), mapping(&field.storage_key)))
                    .collect(),
            };
            Ok(PreparedEnrollment {
                resolved: ResolvedAuthProfile {
                    profile_name: name,
                    provider_id: provider.provider_id.clone(),
                    owner_plugin_id: owner_plugin_id.to_owned(),
                    profile: AuthProfileConfig {
                        backend: "sshenv".to_owned(),
                        provider_id: Some(provider.provider_id.clone()),
                        owner_plugin_id: Some(owner_plugin_id.to_owned()),
                        scheme: Some(method_id.to_owned()),
                        map,
                        settings,
                    },
                    source: AuthProfileSource::Runtime,
                },
                publish_runtime: true,
            })
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_enrollment_binding(
    runtime: &RuntimeAuthSubscriptions,
    provider_id: &str,
    owner: &str,
    name: &str,
) -> Result<(), EnrollmentError> {
    if let Some(binding) = runtime.bindings.get(provider_id)
        && binding.owner_plugin_id != owner
    {
        return Err(AuthProfileResolutionError::OwnerMismatch {
            profile: name.to_owned(),
            expected: owner.to_owned(),
            actual: binding.owner_plugin_id.clone(),
        }
        .into());
    }
    Ok(())
}

/// Allocate a default name for a new account without reusing existing metadata.
/// Selection is advisory: enrollment must still verify ownership at commit time.
#[must_use]
pub fn new_profile_name(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    provider_id: &str,
) -> String {
    let mut occupied: BTreeSet<&str> = config
        .auth
        .profiles
        .keys()
        .map(String::as_str)
        .chain(runtime.profiles.keys().map(String::as_str))
        .collect();
    occupied.extend(
        config
            .auth
            .bindings
            .values()
            .filter_map(|binding| binding.profile.as_deref()),
    );
    occupied.extend(
        runtime
            .bindings
            .values()
            .map(|binding| binding.profile.as_str()),
    );
    occupied.extend(
        config
            .auth
            .pools
            .values()
            .flat_map(|pool| pool.profiles.iter().map(String::as_str)),
    );
    occupied.extend(runtime.pools.values().flat_map(|pool| {
        pool.profiles
            .iter()
            .map(|profile| profile.auth_profile.as_str())
    }));
    if !occupied.contains(provider_id) {
        return provider_id.to_owned();
    }
    for index in 2_u64.. {
        let candidate = format!("{provider_id}-{index}");
        if !occupied.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("profile name space exhausted")
}

fn mapping(key: &str) -> AuthCredentialMapping {
    AuthCredentialMapping {
        env: None,
        key: Some(key.to_owned()),
    }
}

/// Publish the non-secret runtime profile after credential enrollment succeeds.
///
/// # Errors
/// Returns an error for invalid ownership metadata or persistence failures.
pub fn publish(resolved: &ResolvedAuthProfile) -> Result<(), bcode_config::ConfigError> {
    bcode_config::register_runtime_auth_profile(
        &resolved.profile_name,
        bcode_config::RuntimeAuthProfile {
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
            vault: resolved
                .profile
                .settings
                .get("vault")
                .map_or_else(bcode_config::default_auth_vault_path, PathBuf::from),
            map: resolved.profile.map.clone(),
            device_seal: resolved.profile.settings.get("device_seal").cloned(),
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_account_names_do_not_reuse_pool_members_or_dangling_bindings() {
        let mut config = BcodeConfig::default();
        let mut runtime = RuntimeAuthSubscriptions::default();
        assert_eq!(new_profile_name(&config, &runtime, "custom"), "custom");
        config.auth.bindings.insert(
            "provider".to_owned(),
            bcode_config::AuthBindingConfig {
                profile: Some("custom".to_owned()),
            },
        );
        runtime.bindings.insert(
            "provider".to_owned(),
            bcode_config::RuntimeAuthBinding {
                profile: "custom-2".to_owned(),
                owner_plugin_id: "example.plugin".to_owned(),
            },
        );
        config.auth.pools.insert(
            "declared".to_owned(),
            bcode_config::AuthPoolConfig {
                profiles: vec!["custom-3".to_owned()],
                ..Default::default()
            },
        );
        runtime.pools.insert(
            "runtime".to_owned(),
            bcode_config::RuntimeAuthSubscriptionPool {
                profiles: vec![bcode_config::RuntimeAuthSubscriptionProfile {
                    auth_profile: "custom-4".to_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        assert_eq!(new_profile_name(&config, &runtime, "custom"), "custom-5");
        assert_eq!(runtime.profiles.len(), 0);
        assert!(config.auth.profiles.is_empty());
    }

    #[test]
    fn existing_account_method_is_checked_before_interactive_effects() {
        let provider = AuthProviderContribution {
            schema_version: bcode_provider_auth_models::AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
            provider_id: "example".to_owned(),
            display_name: "Example".to_owned(),
            methods: vec![AuthMethodContribution::Interactive {
                method_id: "browser".to_owned(),
                display_name: "Browser".to_owned(),
                operation: "auth.browser".to_owned(),
                credentials: Vec::new(),
                supports_revocation: false,
            }],
        };
        let mut config = BcodeConfig::default();
        config.auth.profiles.insert(
            "account".to_owned(),
            AuthProfileConfig {
                backend: "sshenv".to_owned(),
                provider_id: Some("example".to_owned()),
                owner_plugin_id: Some("bcode.example".to_owned()),
                scheme: Some("different-method".to_owned()),
                map: BTreeMap::new(),
                settings: BTreeMap::new(),
            },
        );
        let destination = EnrollmentDestination {
            profile: Some("account".to_owned()),
            ..EnrollmentDestination::default()
        };
        assert!(matches!(
            prepare(
                &config,
                &RuntimeAuthSubscriptions::default(),
                &provider,
                "bcode.example",
                "browser",
                destination.clone(),
            ),
            Err(EnrollmentError::InvalidMethod)
        ));
        config.auth.profiles.get_mut("account").unwrap().scheme = Some("browser".to_owned());
        let prepared = prepare(
            &config,
            &RuntimeAuthSubscriptions::default(),
            &provider,
            "bcode.example",
            "browser",
            destination,
        )
        .expect("matching owned method");
        assert!(!prepared.publish_runtime);
        let conflict = prepare(
            &config,
            &RuntimeAuthSubscriptions::default(),
            &provider,
            "bcode.example",
            "browser",
            EnrollmentDestination {
                profile: Some("account".to_owned()),
                vault: Some(PathBuf::from("different-vault")),
                recipient_key: None,
            },
        );
        assert!(matches!(
            conflict,
            Err(EnrollmentError::DestinationConflict)
        ));
        let runtime = RuntimeAuthSubscriptions {
            bindings: BTreeMap::from([(
                "example".to_owned(),
                bcode_config::RuntimeAuthBinding {
                    profile: "foreign-account".to_owned(),
                    owner_plugin_id: "foreign.plugin".to_owned(),
                },
            )]),
            ..Default::default()
        };
        assert!(
            prepare(
                &config,
                &runtime,
                &provider,
                "bcode.example",
                "browser",
                EnrollmentDestination {
                    profile: Some("new-account".to_owned()),
                    ..Default::default()
                },
            )
            .is_err()
        );
    }
}
