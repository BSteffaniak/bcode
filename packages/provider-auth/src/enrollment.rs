//! Renderer-independent preparation and publication of owned enrollment profiles.
//!
//! Preparation is read-only. Credential custody and interactive provider effects
//! are separate operations; callers publish metadata only after successful enrollment.

use crate::{AuthProfileResolutionError, AuthProfileSource, ResolvedAuthProfile};
use bcode_config::{
    AuthCredentialMapping, AuthProfileConfig, BcodeConfig, RuntimeAuthSubscriptions,
};
use bcode_provider_auth_models::{AuthMethodContribution, AuthProviderContribution};
use std::collections::BTreeMap;
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
            if let Some(vault) = vault {
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
            let name = profile.unwrap_or_else(|| provider.provider_id.clone());
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

/// Allocate a default name for a new account without reusing existing metadata.
/// Selection is advisory: enrollment must still verify ownership at commit time.
#[must_use]
pub fn new_profile_name(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    provider_id: &str,
) -> String {
    if !config.auth.profiles.contains_key(provider_id)
        && !runtime.profiles.contains_key(provider_id)
    {
        return provider_id.to_owned();
    }
    for index in 2_u64.. {
        let candidate = format!("{provider_id}-{index}");
        if !config.auth.profiles.contains_key(&candidate)
            && !runtime.profiles.contains_key(&candidate)
        {
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
