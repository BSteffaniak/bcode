//! Provider-owned startup discovery. No source grants authentication or consent.

use crate::discovery::{CredentialSourceStatus, read_selected};
pub use bcode_provider_auth_models::startup::{
    StartupAuthCandidate, StartupCredentialAvailability, StartupCredentialSource,
};
use bcode_provider_auth_models::{
    AuthCredentialSource, AuthMethodContribution, AuthProviderContribution,
};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{OnceLock, RwLock};
use zeroize::Zeroizing;

type EnabledProviderDeclarations =
    std::collections::BTreeMap<String, (String, AuthProviderContribution)>;

static ENABLED_PROVIDERS: OnceLock<RwLock<EnabledProviderDeclarations>> = OnceLock::new();

/// Register enabled host-owned declarations for this client process only.
///
/// Callers must obtain ownership from the authenticated plugin host registry, not
/// from durable startup state. Replaces the previous snapshot, so disabled plugins
/// never remain authorized. No credentials are retained.
///
/// # Errors
/// Returns a secret-safe error for malformed or duplicate contributions or poisoned registry state.
pub fn set_enabled_providers<'a>(
    providers: impl IntoIterator<Item = (&'a str, &'a AuthProviderContribution)>,
) -> Result<(), &'static str> {
    let enabled = validated_enabled_providers(providers)?;
    *ENABLED_PROVIDERS
        .get_or_init(|| RwLock::new(std::collections::BTreeMap::new()))
        .write()
        .map_err(|_| "enabled authentication declarations unavailable")? = enabled;
    Ok(())
}

fn validated_enabled_providers<'a>(
    providers: impl IntoIterator<Item = (&'a str, &'a AuthProviderContribution)>,
) -> Result<EnabledProviderDeclarations, &'static str> {
    let mut enabled = std::collections::BTreeMap::new();
    for (plugin_id, contribution) in providers {
        if plugin_id.is_empty() || contribution.validate().is_err() {
            return Err("invalid enabled authentication contribution");
        }
        if enabled
            .insert(
                contribution.provider_id.clone(),
                (plugin_id.to_owned(), contribution.clone()),
            )
            .is_some()
        {
            return Err("duplicate enabled authentication provider identity");
        }
    }
    Ok(enabled)
}

/// Non-secret reference to exactly one consented invocation source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedStartupEnvironment {
    /// Owning authentication provider.
    pub auth_provider_id: String,
    /// Provider-local authentication method.
    pub method_id: String,
    /// Canonical credential field.
    pub credential_id: String,
    /// Exact declared launch variable.
    pub name: String,
}

/// Return remembered source metadata only when startup fallback actually applied.
///
/// # Errors
/// Returns a normalized configuration failure for inconsistent remembered metadata.
pub fn selected_environment_for_selection(
    config: &bcode_config::BcodeConfig,
    selection: &bcode_config::ResolvedModelSelection,
) -> Result<Option<SelectedStartupEnvironment>, bcode_config::ConfigError> {
    selected_environment_with_loader(config, selection, bcode_config::startup::load)
}

fn selected_environment_with_loader(
    config: &bcode_config::BcodeConfig,
    selection: &bcode_config::ResolvedModelSelection,
    load: impl FnOnce() -> Result<
        Option<bcode_config::startup::StartupSelection>,
        bcode_config::startup::StartupStateError,
    >,
) -> Result<Option<SelectedStartupEnvironment>, bcode_config::ConfigError> {
    if !config.startup_selection_applied {
        return Ok(None);
    }
    let state = load()
        .map_err(|_| startup_error("remembered startup selection is unavailable"))?
        .ok_or_else(|| startup_error("remembered startup selection is missing"))?;
    if selection.provider_plugin_id.as_deref() != Some(state.provider_plugin_id.as_str())
        || selection.model_id.as_deref() != Some(state.model_id.as_str())
        || selection.auth_profile != state.auth_profile
        || selection.auth_pool.is_some()
        || config.active_context != state.context
    {
        return Err(startup_error(
            "remembered startup selection changed after configuration was loaded",
        ));
    }
    match (
        state.environment_auth_provider,
        state.environment_method,
        state.environment_credential,
        state.environment_variable,
    ) {
        (Some(auth_provider_id), Some(method_id), Some(credential_id), Some(name)) => {
            Ok(Some(SelectedStartupEnvironment {
                auth_provider_id,
                method_id,
                credential_id,
                name,
            }))
        }
        (None, None, None, None) => Ok(None),
        _ => Err(startup_error(
            "remembered environment selection is incomplete",
        )),
    }
}

pub(crate) fn resolve_remembered_environment(
    config: &bcode_config::BcodeConfig,
    selection: &bcode_config::ResolvedModelSelection,
) -> Result<Option<bcode_model::ProviderRequestContext>, bcode_config::ConfigError> {
    let Some(source) = selected_environment_for_selection(config, selection)? else {
        return Ok(None);
    };
    let enabled = ENABLED_PROVIDERS
        .get()
        .ok_or_else(|| {
            startup_error("selected environment provider must be verified by the application host")
        })?
        .read()
        .map_err(|_| startup_error("enabled authentication declarations unavailable"))?;
    resolve_from_enabled(&enabled, selection, &source, |name| {
        std::env::var(name).ok().map(Zeroizing::new)
    })
    .map(Some)
}

fn resolve_from_enabled(
    enabled: &EnabledProviderDeclarations,
    selection: &bcode_config::ResolvedModelSelection,
    source: &SelectedStartupEnvironment,
    environment: impl FnMut(&str) -> Option<Zeroizing<String>>,
) -> Result<bcode_model::ProviderRequestContext, bcode_config::ConfigError> {
    let (owner, contribution) = enabled
        .get(&source.auth_provider_id)
        .ok_or_else(|| startup_error("selected environment provider is disabled or unavailable"))?;
    if selection.provider_plugin_id.as_deref() != Some(owner.as_str()) {
        return Err(startup_error(
            "selected environment provider ownership is inconsistent",
        ));
    }
    let mut context = resolve_selected_environment(
        contribution,
        &source.method_id,
        &source.credential_id,
        &source.name,
        environment,
    )
    .map_err(startup_error)?;
    context.settings.clone_from(&selection.settings);
    context.model_profile.clone_from(&selection.model_profile);
    Ok(context)
}

fn startup_error(message: &str) -> bcode_config::ConfigError {
    bcode_config::ConfigError::Composition {
        message: message.to_owned(),
    }
}

/// Discover sanitized choices from enabled, validated provider contributions.
///
/// The caller supplies the launch environment reader; only declared variable names
/// are requested. Disabled discovery returns without reading files or variables.
/// Every candidate remains a separate choice, including multiple billing accounts.
#[must_use]
pub fn discover_candidates<'a>(
    providers: impl IntoIterator<Item = &'a AuthProviderContribution>,
    enabled: bool,
    home: &Path,
    mut environment: impl FnMut(&str) -> Option<Zeroizing<String>>,
) -> Vec<StartupAuthCandidate> {
    if !enabled {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for provider in providers {
        if provider.validate().is_err() {
            continue;
        }
        for method in &provider.methods {
            let AuthMethodContribution::SecretFields {
                fields,
                display_name,
                ..
            } = method
            else {
                continue;
            };
            for field in fields {
                let completes_method = fields
                    .iter()
                    .all(|other| other.optional || other.credential_id == field.credential_id);
                let mut seen_environment = BTreeSet::new();
                for name in &field.invocation_env {
                    seen_environment.insert(name.as_str());
                    let availability = environment_status(environment(name), field);
                    candidates.push(StartupAuthCandidate {
                        provider_id: provider.provider_id.clone(),
                        provider_name: provider.display_name.clone(),
                        method_id: method.method_id().to_owned(),
                        method_name: display_name.clone(),
                        credential_id: field.credential_id.clone(),
                        source: StartupCredentialSource::Environment { name: name.clone() },
                        availability,
                        completes_method,
                    });
                }
                for (source_index, source) in field.discovery_sources.iter().enumerate() {
                    let (label, availability) = match source {
                        AuthCredentialSource::Environment { name } => {
                            if seen_environment.contains(name.as_str()) {
                                continue;
                            }
                            (name.clone(), environment_status(environment(name), field))
                        }
                        AuthCredentialSource::JsonFile { application, .. } => {
                            let status = match read_selected(home, field, source_index) {
                                Ok(Some(_)) => StartupCredentialAvailability::Available,
                                Ok(None) => StartupCredentialAvailability::Missing,
                                Err(CredentialSourceStatus::Missing) => {
                                    StartupCredentialAvailability::Missing
                                }
                                Err(_) => StartupCredentialAvailability::Unavailable,
                            };
                            (application.clone(), status)
                        }
                    };
                    candidates.push(StartupAuthCandidate {
                        provider_id: provider.provider_id.clone(),
                        provider_name: provider.display_name.clone(),
                        method_id: method.method_id().to_owned(),
                        method_name: display_name.clone(),
                        credential_id: field.credential_id.clone(),
                        source: StartupCredentialSource::Import {
                            source_index,
                            label,
                        },
                        availability,
                        completes_method,
                    });
                }
            }
        }
    }
    candidates
}

/// Resolve exactly one consented environment source into request-local auth.
///
/// The source must be an invocation declaration of the selected method. No other
/// variable or discovery file is read; multi-field methods require enrollment.
///
/// # Errors
/// Returns secret-free errors for invalid declarations, incomplete methods, or
/// absent/invalid selected credentials.
pub fn resolve_selected_environment(
    contribution: &AuthProviderContribution,
    method_id: &str,
    credential_id: &str,
    name: &str,
    mut environment: impl FnMut(&str) -> Option<Zeroizing<String>>,
) -> Result<bcode_model::ProviderRequestContext, &'static str> {
    contribution
        .validate()
        .map_err(|_| "invalid provider auth declaration")?;
    let method = contribution
        .methods
        .iter()
        .find(|method| method.method_id() == method_id)
        .ok_or("selected authentication method is unavailable")?;
    let AuthMethodContribution::SecretFields { fields, .. } = method else {
        return Err("selected authentication method requires enrollment");
    };
    let field = fields
        .iter()
        .find(|field| field.credential_id == credential_id)
        .ok_or("selected credential field is unavailable")?;
    if !field.invocation_env.iter().any(|declared| declared == name)
        || fields
            .iter()
            .any(|other| !other.optional && other.credential_id != credential_id)
    {
        return Err("selected environment source requires enrollment");
    }
    let value = environment(name).ok_or("selected environment credential is missing")?;
    if value.trim().is_empty() || field.validation.validate_secret(&value).is_err() {
        return Err("selected environment credential is invalid");
    }
    let auth = bcode_model::ProviderAuthContext {
        profile: Some("environment".to_owned()),
        backend: Some("environment".to_owned()),
        scheme: Some(method_id.to_owned()),
        credentials: std::collections::BTreeMap::from([(
            credential_id.to_owned(),
            bcode_model::ProviderAuthCredential {
                value: value.to_string(),
                source: Some(name.to_owned()),
            },
        )]),
        ..Default::default()
    };
    Ok(bcode_model::ProviderRequestContext {
        auth_profile: Some("environment".to_owned()),
        auth: Some(auth),
        ..Default::default()
    })
}

fn environment_status(
    value: Option<Zeroizing<String>>,
    field: &bcode_provider_auth_models::AuthSecretField,
) -> StartupCredentialAvailability {
    match value {
        None => StartupCredentialAvailability::Missing,
        Some(value) if value.trim().is_empty() => StartupCredentialAvailability::Missing,
        Some(value) if field.validation.validate_secret(&value).is_ok() => {
            StartupCredentialAvailability::Available
        }
        Some(_) => StartupCredentialAvailability::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_provider_auth_models::{
        AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION, AuthSecretField, AuthSecretValidation,
    };

    fn provider() -> AuthProviderContribution {
        AuthProviderContribution {
            schema_version: AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
            provider_id: "fake".to_owned(),
            display_name: "Fake".to_owned(),
            methods: vec![AuthMethodContribution::SecretFields {
                method_id: "key".to_owned(),
                display_name: "API key".to_owned(),
                fields: vec![AuthSecretField {
                    credential_id: "api_key".to_owned(),
                    storage_key: "API_KEY".to_owned(),
                    prompt: "Key".to_owned(),
                    optional: false,
                    validation: AuthSecretValidation::default(),
                    invocation_env: vec!["FAKE_PRIMARY".to_owned(), "FAKE_SECONDARY".to_owned()],
                    discovery_sources: vec![AuthCredentialSource::Environment {
                        name: "FAKE_PRIMARY".to_owned(),
                    }],
                }],
                supports_verification: false,
                supports_revocation: false,
            }],
        }
    }

    #[test]
    fn disabled_discovery_never_reads_environment() {
        assert!(
            discover_candidates([&provider()], false, Path::new("/missing"), |_| panic!(
                "must not read"
            ))
            .is_empty()
        );
    }

    #[test]
    fn account_sources_remain_separate_and_values_never_leave_discovery() {
        let candidates = discover_candidates([&provider()], true, Path::new("/missing"), |_| {
            Some(Zeroizing::new("synthetic-test-value".to_owned()))
        });
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|candidate| candidate.availability
            == StartupCredentialAvailability::Available
            && candidate.completes_method));
        let encoded = serde_json::to_string(&candidates).unwrap();
        assert!(!encoded.contains("synthetic-test-value"));
        assert_ne!(candidates[0].source, candidates[1].source);
    }

    #[test]
    fn explicitly_selected_account_does_not_read_other_account() {
        let context =
            resolve_selected_environment(&provider(), "key", "api_key", "FAKE_SECONDARY", |name| {
                assert_eq!(name, "FAKE_SECONDARY");
                Some(Zeroizing::new("selected-synthetic-value".to_owned()))
            })
            .unwrap();
        assert_eq!(
            context.auth.unwrap().credentials["api_key"]
                .source
                .as_deref(),
            Some("FAKE_SECONDARY")
        );
        assert!(
            resolve_selected_environment(&provider(), "key", "api_key", "UNDECLARED", |_| panic!(
                "must not read"
            ))
            .is_err()
        );
        assert!(
            resolve_selected_environment(&provider(), "key", "api_key", "FAKE_SECONDARY", |_| None)
                .is_err()
        );
    }

    #[test]
    fn disabled_or_foreign_provider_never_reads_selected_environment() {
        let source = SelectedStartupEnvironment {
            auth_provider_id: "fake".to_owned(),
            method_id: "key".to_owned(),
            credential_id: "api_key".to_owned(),
            name: "FAKE_SECONDARY".to_owned(),
        };
        let selection = bcode_config::ResolvedModelSelection {
            provider_plugin_id: Some("fake.plugin".to_owned()),
            ..Default::default()
        };
        let empty = EnabledProviderDeclarations::new();
        assert!(
            resolve_from_enabled(&empty, &selection, &source, |_| panic!(
                "disabled provider must not read"
            ))
            .is_err()
        );
        let foreign = EnabledProviderDeclarations::from([(
            "fake".to_owned(),
            ("other.plugin".to_owned(), provider()),
        )]);
        assert!(
            resolve_from_enabled(&foreign, &selection, &source, |_| panic!(
                "foreign provider must not read"
            ))
            .is_err()
        );
        let enabled = EnabledProviderDeclarations::from([(
            "fake".to_owned(),
            ("fake.plugin".to_owned(), provider()),
        )]);
        assert!(resolve_from_enabled(&enabled, &selection, &source, |_| None).is_err());
        let context = resolve_from_enabled(&enabled, &selection, &source, |name| {
            assert_eq!(name, "FAKE_SECONDARY");
            Some(Zeroizing::new("synthetic-selected".to_owned()))
        })
        .unwrap();
        assert!(context.env.is_empty());
        assert!(context.auth_candidates.is_empty());
        assert_eq!(
            context.auth.unwrap().credentials["api_key"].value,
            "synthetic-selected"
        );
    }

    #[test]
    fn changed_or_removed_state_fails_before_source_resolution() {
        let state = bcode_config::startup::StartupSelection {
            provider_plugin_id: "fake.plugin".to_owned(),
            model_id: "exact".to_owned(),
            base_url: None,
            dialect: None,
            auth_mode: None,
            catalog_provider_id: None,
            auth_profile: None,
            context: None,
            environment_auth_provider: Some("fake".to_owned()),
            environment_method: Some("key".to_owned()),
            environment_credential: Some("api_key".to_owned()),
            environment_variable: Some("FAKE_PRIMARY".to_owned()),
        };
        let config = bcode_config::BcodeConfig {
            startup_selection_applied: true,
            ..Default::default()
        };
        let selection = bcode_config::ResolvedModelSelection {
            provider_plugin_id: Some("fake.plugin".to_owned()),
            model_id: Some("exact".to_owned()),
            ..Default::default()
        };
        assert!(selected_environment_with_loader(&config, &selection, || Ok(None)).is_err());
        assert!(
            selected_environment_with_loader(&config, &selection, || Ok(Some(state.clone())))
                .unwrap()
                .is_some()
        );
        for change in 0..4 {
            let mut changed = state.clone();
            match change {
                0 => changed.provider_plugin_id = "foreign.plugin".to_owned(),
                1 => changed.model_id = "different".to_owned(),
                2 => changed.auth_profile = Some("another-account".to_owned()),
                _ => changed.context = Some("different-context".to_owned()),
            }
            let result =
                selected_environment_with_loader(&config, &selection, || Ok(Some(changed)));
            assert!(result.is_err());
            // Credential resolution is permitted only after metadata matching succeeds.
            let source: Result<Option<()>, _> = result.and_then(|source| {
                source.map_or(Ok(None), |_| {
                    panic!("stale state must not reach source reads")
                })
            });
            assert!(source.is_err());
        }
        let explicit = bcode_config::BcodeConfig::default();
        assert!(
            selected_environment_with_loader(&explicit, &selection, || panic!(
                "explicit config must not reread state"
            ))
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn duplicate_provider_identity_is_rejected_without_publishing_snapshot() {
        let first = provider();
        let second = provider();
        assert!(
            validated_enabled_providers([("first.plugin", &first), ("second.plugin", &second)])
                .is_err()
        );
        assert!(
            validated_enabled_providers([("first.plugin", &first), ("first.plugin", &first)])
                .is_err()
        );
        assert_eq!(
            validated_enabled_providers([("first.plugin", &first)])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn explicit_configuration_never_loads_remembered_environment() {
        let config = bcode_config::BcodeConfig::default();
        assert_eq!(
            selected_environment_for_selection(&config, &config.resolved_model_selection())
                .unwrap(),
            None
        );
    }

    #[test]
    fn multiple_required_fields_cannot_be_connected_by_one_source() {
        let mut provider = provider();
        let AuthMethodContribution::SecretFields { fields, .. } = &mut provider.methods[0] else {
            unreachable!()
        };
        let mut second = fields[0].clone();
        second.credential_id = "tenant".to_owned();
        second.storage_key = "TENANT".to_owned();
        second.invocation_env = vec!["FAKE_TENANT".to_owned()];
        second.discovery_sources.clear();
        fields.push(second);
        let candidates = discover_candidates([&provider], true, Path::new("/missing"), |_| {
            Some(Zeroizing::new("synthetic".to_owned()))
        });
        assert!(
            candidates
                .iter()
                .all(|candidate| !candidate.completes_method)
        );
        assert!(
            resolve_selected_environment(&provider, "key", "api_key", "FAKE_PRIMARY", |_| panic!(
                "incomplete method must not read"
            ))
            .is_err()
        );
    }

    #[test]
    fn invalid_contributions_are_not_discovered() {
        let mut provider = provider();
        provider.schema_version += 1;
        assert!(
            discover_candidates([&provider], true, Path::new("/missing"), |_| panic!(
                "must not read"
            ))
            .is_empty()
        );
    }

    #[test]
    fn discovery_hint_does_not_grant_invocation_authority() {
        let mut provider = provider();
        let AuthMethodContribution::SecretFields { fields, .. } = &mut provider.methods[0] else {
            unreachable!()
        };
        fields[0].invocation_env.clear();
        let candidates = discover_candidates([&provider], true, Path::new("/missing"), |_| {
            Some(Zeroizing::new("synthetic".to_owned()))
        });
        assert!(matches!(
            candidates[0].source,
            StartupCredentialSource::Import { .. }
        ));
    }
}
