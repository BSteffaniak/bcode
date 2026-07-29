//! Generic, ownership-checked authentication vault lifecycle operations.

use crate::{AuthProfileResolutionError, ResolvedAuthProfile};
use bcode_provider_auth_models::AuthMethodContribution;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Structured non-secret lifecycle diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthVaultDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub remediation: Option<String>,
}

/// Result of inspecting an owned vault profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthVaultProfileStatus {
    pub profile_exists: bool,
    pub present_credentials: BTreeSet<String>,
    pub diagnostics: Vec<AuthVaultDiagnostic>,
}

/// Ownership-checked vault operation failure.
#[derive(Debug, thiserror::Error)]
pub enum AuthVaultLifecycleError {
    #[error(transparent)]
    Ownership(#[from] AuthProfileResolutionError),
    #[error("auth method '{method_id}' is not registered for provider '{provider_id}'")]
    UnknownMethod {
        provider_id: String,
        method_id: String,
    },
    #[error("auth credential '{credential_id}' is not owned by method '{method_id}'")]
    UnknownCredential {
        method_id: String,
        credential_id: String,
    },
    #[error("auth profile backend '{0}' is unsupported for integrated vault operations")]
    UnsupportedBackend(String),
    #[error("auth profile has no authentication scheme")]
    MissingScheme,
    #[error("auth profile scheme '{actual}' does not match selected method '{expected}'")]
    SchemeMismatch { expected: String, actual: String },
    #[error("auth vault is unavailable: {0}")]
    VaultUnavailable(String),
    #[error("auth vault profile is unavailable: {0}")]
    ProfileUnavailable(String),
    #[error("auth vault write failed: {0}")]
    WriteFailed(String),
    #[error("auth device-seal requirement is not satisfied")]
    DeviceSealRequired(Vec<crate::security::AuthSecurityDiagnostic>),
}

/// Host-owned lifecycle service for one already-resolved provider profile and method.
pub struct AuthVaultLifecycle<'a> {
    resolved: &'a ResolvedAuthProfile,
    method: &'a AuthMethodContribution,
}

impl<'a> AuthVaultLifecycle<'a> {
    /// Construct a lifecycle service after validating provider, plugin, profile, scheme, method,
    /// and credential ownership.
    ///
    /// # Errors
    ///
    /// Returns an error when method/profile ownership or scheme is inconsistent.
    pub fn new(
        resolved: &'a ResolvedAuthProfile,
        registered_provider_id: &str,
        registered_owner_plugin_id: &str,
        method: &'a AuthMethodContribution,
    ) -> Result<Self, AuthVaultLifecycleError> {
        validate_resolved_owner(resolved, registered_provider_id, registered_owner_plugin_id)?;
        if resolved.profile.backend != "sshenv" {
            return Err(AuthVaultLifecycleError::UnsupportedBackend(
                resolved.profile.backend.clone(),
            ));
        }
        let scheme = resolved
            .profile
            .scheme
            .as_deref()
            .ok_or(AuthVaultLifecycleError::MissingScheme)?;
        if scheme != method.method_id() {
            return Err(AuthVaultLifecycleError::SchemeMismatch {
                expected: method.method_id().to_owned(),
                actual: scheme.to_owned(),
            });
        }
        Ok(Self { resolved, method })
    }

    /// Inspect owned credential presence without returning secret values.
    ///
    /// # Errors
    ///
    /// Returns an error when the vault or selected profile cannot be read. No mutation or fallback
    /// occurs on failure.
    pub fn inspect(&self) -> Result<AuthVaultProfileStatus, AuthVaultLifecycleError> {
        let vault = self.vault_path();
        if !vault.exists() {
            return Ok(AuthVaultProfileStatus {
                profile_exists: false,
                present_credentials: BTreeSet::new(),
                diagnostics: vec![AuthVaultDiagnostic {
                    code: "auth_vault_missing",
                    message: format!("Auth vault at {} does not exist.", vault.display()),
                    remediation: Some("Run the provider login command to create it.".to_owned()),
                }],
            });
        }
        let profile = crate::security::read_auth_vault_profile(&vault, self.storage_profile())
            .map_err(AuthVaultLifecycleError::ProfileUnavailable)?;
        let Some(values) = profile else {
            return Ok(AuthVaultProfileStatus {
                profile_exists: false,
                present_credentials: BTreeSet::new(),
                diagnostics: vec![AuthVaultDiagnostic {
                    code: "auth_vault_profile_missing",
                    message: format!(
                        "Auth vault profile '{}' does not exist.",
                        self.storage_profile()
                    ),
                    remediation: Some("Run the provider login command.".to_owned()),
                }],
            });
        };
        let present_credentials = self
            .credential_storage_keys()?
            .into_iter()
            .filter_map(|(credential, key)| values.contains_key(&key).then_some(credential))
            .collect();
        Ok(AuthVaultProfileStatus {
            profile_exists: true,
            present_credentials,
            diagnostics: Vec::new(),
        })
    }

    /// Read owned credentials into canonical credential IDs.
    ///
    /// # Errors
    ///
    /// Returns an error when the vault/profile cannot be read or declared ownership is invalid.
    /// Damaged state never causes reset, mutation, or fallback.
    pub fn read(&self) -> Result<BTreeMap<String, String>, AuthVaultLifecycleError> {
        let vault = self.vault_path();
        if !vault.exists() {
            return Err(AuthVaultLifecycleError::VaultUnavailable(format!(
                "vault at {} does not exist",
                vault.display()
            )));
        }
        let values = crate::security::read_auth_vault_profile(&vault, self.storage_profile())
            .map_err(AuthVaultLifecycleError::ProfileUnavailable)?
            .ok_or_else(|| {
                AuthVaultLifecycleError::ProfileUnavailable(format!(
                    "profile '{}' does not exist",
                    self.storage_profile()
                ))
            })?;
        Ok(self
            .credential_storage_keys()?
            .into_iter()
            .filter_map(|(credential, key)| {
                values.get(&key).map(|value| (credential, value.to_owned()))
            })
            .collect())
    }

    /// Upsert only credentials declared by the selected provider method.
    ///
    /// # Errors
    ///
    /// Returns an error before mutation for undeclared credentials, damaged vault/profile state,
    /// write failure, or unsatisfied required device-seal policy.
    pub fn upsert(
        &self,
        credentials: BTreeMap<String, String>,
    ) -> Result<Vec<crate::security::AuthSecurityDiagnostic>, AuthVaultLifecycleError> {
        let storage_keys = self.credential_storage_keys()?;
        for credential in credentials.keys() {
            if !storage_keys.contains_key(credential) {
                return Err(AuthVaultLifecycleError::UnknownCredential {
                    method_id: self.method.method_id().to_owned(),
                    credential_id: credential.clone(),
                });
            }
        }
        let (store, recipient_key) = self.open_or_initialize_store()?;
        let mut values = match store.get_profile(self.storage_profile()) {
            Ok(Some(values)) => values,
            Ok(None) => BTreeMap::new(),
            Err(error) => {
                return Err(AuthVaultLifecycleError::ProfileUnavailable(
                    error.to_string(),
                ));
            }
        };
        for (credential, value) in credentials {
            let key = &storage_keys[&credential];
            values.insert(key.clone(), Zeroizing::new(value));
        }
        store
            .replace_profile(self.storage_profile(), values)
            .map_err(|error| AuthVaultLifecycleError::WriteFailed(error.to_string()))?;
        self.reconcile_device_seal(Some(&recipient_key))
    }

    /// Delete only credentials declared by the selected provider method.
    ///
    /// Other values and credentials in the same profile are preserved. Missing profiles are
    /// idempotent. Damaged profiles fail without mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the vault/profile cannot be read or the resulting profile cannot be
    /// written.
    pub fn delete(&self) -> Result<(), AuthVaultLifecycleError> {
        let vault = self.vault_path();
        if !vault.exists() {
            return Ok(());
        }
        let store = auth_store(&vault);
        let Some(mut values) = store
            .get_profile(self.storage_profile())
            .map_err(|error| AuthVaultLifecycleError::ProfileUnavailable(error.to_string()))?
        else {
            return Ok(());
        };
        for key in self.credential_storage_keys()?.into_values() {
            values.remove(&key);
        }
        store
            .replace_profile(self.storage_profile(), values)
            .map_err(|error| AuthVaultLifecycleError::WriteFailed(error.to_string()))
    }

    fn credential_storage_keys(&self) -> Result<BTreeMap<String, String>, AuthVaultLifecycleError> {
        let AuthMethodContribution::SecretFields { fields, .. } = self.method else {
            return Err(AuthVaultLifecycleError::UnknownMethod {
                provider_id: self.resolved.provider_id.clone(),
                method_id: self.method.method_id().to_owned(),
            });
        };
        let mut keys = BTreeMap::new();
        for field in fields {
            let configured = self
                .resolved
                .profile
                .map
                .get(&field.credential_id)
                .and_then(|mapping| mapping.key.as_ref().or(mapping.env.as_ref()))
                .cloned()
                .unwrap_or_else(|| field.storage_key.clone());
            keys.insert(field.credential_id.clone(), configured);
        }
        Ok(keys)
    }

    fn storage_profile(&self) -> &str {
        self.resolved
            .profile
            .settings
            .get("profile")
            .map_or(self.resolved.profile_name.as_str(), String::as_str)
    }

    fn vault_path(&self) -> PathBuf {
        self.resolved
            .profile
            .settings
            .get("vault")
            .map_or_else(bcode_config::default_auth_vault_path, PathBuf::from)
    }

    fn open_or_initialize_store(
        &self,
    ) -> Result<(sshenv_vault::SshenvStore, String), AuthVaultLifecycleError> {
        let vault = self.vault_path();
        let recipient_key = crate::security::ensure_vault_recipient_key(&vault)
            .map_err(|error| AuthVaultLifecycleError::VaultUnavailable(error.to_string()))?;
        let store = auth_store(&vault);
        if vault.exists() {
            // Validate existing state before any write. Never archive/reset damaged vaults here.
            sshenv_vault::load_and_unlock_metadata_with_private_key_paths(
                &vault,
                &crate::security::vault_private_key_paths(&vault),
            )
            .map_err(|error| AuthVaultLifecycleError::VaultUnavailable(error.to_string()))?;
        } else {
            initialize_vault(&vault, &store, &recipient_key)?;
        }
        Ok((store, recipient_key))
    }

    fn reconcile_device_seal(
        &self,
        recipient_key: Option<&str>,
    ) -> Result<Vec<crate::security::AuthSecurityDiagnostic>, AuthVaultLifecycleError> {
        let report = crate::security::reconcile_auth_vault_security_report_with_options(
            &self.vault_path(),
            self.storage_profile(),
            crate::security::device_seal_options_for_auth_profile(&self.resolved.profile),
            recipient_key,
        );
        if report.diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == crate::security::AuthSecurityDiagnosticSeverity::Error
        }) {
            return Err(AuthVaultLifecycleError::DeviceSealRequired(
                report.diagnostics,
            ));
        }
        Ok(report.diagnostics)
    }
}

fn validate_resolved_owner(
    resolved: &ResolvedAuthProfile,
    provider_id: &str,
    owner_plugin_id: &str,
) -> Result<(), AuthProfileResolutionError> {
    if resolved.provider_id != provider_id {
        return Err(AuthProfileResolutionError::ProviderMismatch {
            profile: resolved.profile_name.clone(),
            expected: provider_id.to_owned(),
            actual: resolved.provider_id.clone(),
        });
    }
    if resolved.owner_plugin_id != owner_plugin_id {
        return Err(AuthProfileResolutionError::OwnerMismatch {
            profile: resolved.profile_name.clone(),
            expected: owner_plugin_id.to_owned(),
            actual: resolved.owner_plugin_id.clone(),
        });
    }
    super::validate_auth_profile_ownership(
        &resolved.profile_name,
        &resolved.profile,
        provider_id,
        owner_plugin_id,
    )
}

fn auth_store(vault: &Path) -> sshenv_vault::SshenvStore {
    sshenv_vault::SshenvStore::new(
        sshenv_vault::SshenvStoreConfig::new(vault.to_path_buf())
            .with_private_key_paths(crate::security::vault_private_key_paths(vault)),
    )
}

fn initialize_vault(
    vault_path: &Path,
    store: &sshenv_vault::SshenvStore,
    recipient_key: &str,
) -> Result<(), AuthVaultLifecycleError> {
    store
        .init(recipient_key)
        .map_err(|error| AuthVaultLifecycleError::WriteFailed(error.to_string()))?;
    let (mut vault, data_key) = store
        .load_and_unlock()
        .map_err(|error| AuthVaultLifecycleError::VaultUnavailable(error.to_string()))?;
    vault
        .migrate_to_v2(&[recipient_key.to_owned()])
        .map_err(|error| AuthVaultLifecycleError::WriteFailed(error.to_string()))?;
    vault
        .enable_profile_keys()
        .map_err(|error| AuthVaultLifecycleError::WriteFailed(error.to_string()))?;
    vault
        .save(vault_path, &data_key)
        .map_err(|error| AuthVaultLifecycleError::WriteFailed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_provider_auth_models::{AuthSecretField, AuthSecretValidation};

    fn resolved(vault: &Path) -> ResolvedAuthProfile {
        ResolvedAuthProfile {
            profile_name: "exa".to_owned(),
            provider_id: "exa".to_owned(),
            owner_plugin_id: "bcode.web-search".to_owned(),
            profile: bcode_config::AuthProfileConfig {
                backend: "sshenv".to_owned(),
                provider_id: Some("exa".to_owned()),
                owner_plugin_id: Some("bcode.web-search".to_owned()),
                scheme: Some("api_key".to_owned()),
                map: BTreeMap::new(),
                settings: BTreeMap::from([
                    ("profile".to_owned(), "exa".to_owned()),
                    ("vault".to_owned(), vault.display().to_string()),
                    ("device_seal".to_owned(), "off".to_owned()),
                ]),
            },
            source: crate::AuthProfileSource::Declarative,
        }
    }

    fn method() -> AuthMethodContribution {
        AuthMethodContribution::SecretFields {
            method_id: "api_key".to_owned(),
            display_name: "API key".to_owned(),
            fields: vec![AuthSecretField {
                credential_id: "api_key".to_owned(),
                storage_key: "EXA_API_KEY".to_owned(),
                prompt: "Exa API key".to_owned(),
                optional: false,
                validation: AuthSecretValidation::default(),
            }],
            supports_verification: false,
            supports_revocation: false,
        }
    }

    #[test]
    fn ownership_and_scheme_are_checked_before_vault_access() {
        let temp = tempfile::tempdir().expect("tempdir");
        let resolved = resolved(&temp.path().join("vault"));
        assert!(matches!(
            AuthVaultLifecycle::new(&resolved, "exa", "bcode.other", &method()),
            Err(AuthVaultLifecycleError::Ownership(
                AuthProfileResolutionError::OwnerMismatch { .. }
            ))
        ));

        let mut wrong_scheme = resolved;
        wrong_scheme.profile.scheme = Some("oauth".to_owned());
        assert!(matches!(
            AuthVaultLifecycle::new(&wrong_scheme, "exa", "bcode.web-search", &method()),
            Err(AuthVaultLifecycleError::SchemeMismatch { .. })
        ));
    }

    #[test]
    fn upsert_read_targeted_delete_and_status_preserve_unrelated_values() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vault = temp.path().join("vault");
        let resolved = resolved(&vault);
        let method = method();
        let lifecycle = AuthVaultLifecycle::new(&resolved, "exa", "bcode.web-search", &method)
            .expect("lifecycle");
        lifecycle
            .upsert(BTreeMap::from([(
                "api_key".to_owned(),
                "secret".to_owned(),
            )]))
            .expect("upsert");

        assert_eq!(
            lifecycle.read().expect("read").get("api_key"),
            Some(&"secret".to_owned())
        );
        assert!(
            lifecycle
                .inspect()
                .expect("inspect")
                .present_credentials
                .contains("api_key")
        );

        let store = auth_store(&vault);
        let mut raw = store
            .get_profile("exa")
            .expect("read raw")
            .expect("profile");
        raw.insert("UNRELATED".to_owned(), Zeroizing::new("keep".to_owned()));
        store.replace_profile("exa", raw).expect("add unrelated");

        lifecycle.delete().expect("targeted delete");
        let raw = store
            .get_profile("exa")
            .expect("read raw")
            .expect("profile");
        assert!(!raw.contains_key("EXA_API_KEY"));
        assert_eq!(
            raw.get("UNRELATED").map(|value| value.as_str()),
            Some("keep")
        );
    }

    #[test]
    fn required_local_file_device_seal_is_applied_and_reported() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vault = temp.path().join("vault");
        let mut resolved = resolved(&vault);
        resolved
            .profile
            .settings
            .insert("device_seal".to_owned(), "required".to_owned());
        resolved
            .profile
            .settings
            .insert("device_seal_backend".to_owned(), "local-file".to_owned());
        resolved
            .profile
            .settings
            .insert("device_seal_strict".to_owned(), "true".to_owned());
        let method = method();
        let lifecycle = AuthVaultLifecycle::new(&resolved, "exa", "bcode.web-search", &method)
            .expect("lifecycle");

        lifecycle
            .upsert(BTreeMap::from([(
                "api_key".to_owned(),
                "secret".to_owned(),
            )]))
            .expect("sealed upsert");
        let status = crate::security::inspect_auth_vault_security(
            &vault,
            "exa",
            crate::security::AuthDeviceSealPolicy::Required,
        );

        assert!(status.profile_exists);
        assert!(status.profile_device_sealed);
        assert_eq!(status.device_seal_backend.as_deref(), Some("local-file"));
    }

    #[test]
    fn damaged_vault_fails_without_resetting_or_mutating_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vault = temp.path().join("vault");
        std::fs::write(&vault, b"damaged-vault").expect("write damaged vault");
        let before = std::fs::read(&vault).expect("read before");
        let resolved = resolved(&vault);
        let method = method();
        let lifecycle = AuthVaultLifecycle::new(&resolved, "exa", "bcode.web-search", &method)
            .expect("lifecycle");

        assert!(matches!(
            lifecycle.upsert(BTreeMap::from([(
                "api_key".to_owned(),
                "secret".to_owned()
            )])),
            Err(AuthVaultLifecycleError::VaultUnavailable(_))
        ));
        assert_eq!(std::fs::read(&vault).expect("read after"), before);
    }

    #[test]
    fn undeclared_credentials_fail_before_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vault = temp.path().join("vault");
        let resolved = resolved(&vault);
        let method = method();
        let lifecycle = AuthVaultLifecycle::new(&resolved, "exa", "bcode.web-search", &method)
            .expect("lifecycle");

        assert!(matches!(
            lifecycle.upsert(BTreeMap::from([(
                "refresh_token".to_owned(),
                "secret".to_owned()
            )])),
            Err(AuthVaultLifecycleError::UnknownCredential { .. })
        ));
        assert!(!vault.exists());
    }
}
