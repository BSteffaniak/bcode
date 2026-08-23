//! Host-owned provider-auth application operations.

use crate::{AuthProfileResolutionError, ResolvedAuthProfile, lifecycle::AuthVaultLifecycle};
use bcode_provider_auth_models::{
    AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION, AUTH_SECURITY_INSPECTION_SCHEMA_VERSION,
    AuthCredentialUpdateRequest, AuthCredentialUpdateResponse, AuthDiagnostic,
    AuthDiagnosticSeverity, AuthMethodContribution, AuthSecurityInspectionRequest,
    AuthSecurityInspectionResponse,
};

/// Authorization context bound by the host from an active plugin/provider invocation.
#[derive(Clone, Copy)]
pub struct AuthCredentialUpdateContext<'a> {
    pub caller_plugin_id: &'a str,
    pub provider_id: &'a str,
    pub resolved: &'a ResolvedAuthProfile,
    pub method: &'a AuthMethodContribution,
}

/// Host credential-update failure.
#[derive(Debug, thiserror::Error)]
pub enum AuthCredentialUpdateError {
    #[error("invalid credential update request: {0}")]
    InvalidRequest(String),
    #[error(transparent)]
    Ownership(#[from] AuthProfileResolutionError),
    #[error(transparent)]
    Lifecycle(#[from] crate::lifecycle::AuthVaultLifecycleError),
}

/// Persist refreshed credentials through host-owned vault custody.
///
/// The caller cannot provide an owner identity, vault path, storage profile, or backend key.
/// Host-bound ownership and the registered method are validated before vault access.
///
/// # Errors
///
/// Returns an error for invalid payloads, profile mismatch, ownership mismatch, undeclared
/// credentials, damaged vault state, device-seal failure, or write failure.
pub fn update_credentials(
    context: AuthCredentialUpdateContext<'_>,
    request: AuthCredentialUpdateRequest,
) -> Result<AuthCredentialUpdateResponse, AuthCredentialUpdateError> {
    request
        .validate()
        .map_err(|error| AuthCredentialUpdateError::InvalidRequest(error.to_string()))?;
    if request.provider_id != context.provider_id {
        return Err(AuthCredentialUpdateError::Ownership(
            AuthProfileResolutionError::ProviderMismatch {
                profile: request.profile,
                expected: context.provider_id.to_owned(),
                actual: request.provider_id,
            },
        ));
    }
    if request.profile != context.resolved.profile_name {
        return Err(AuthCredentialUpdateError::Ownership(
            AuthProfileResolutionError::MissingProfile {
                provider_id: context.provider_id.to_owned(),
                profile: request.profile,
            },
        ));
    }
    let mut updated_credentials = request.credentials.keys().cloned().collect::<Vec<_>>();
    updated_credentials.sort();
    AuthVaultLifecycle::new(
        context.resolved,
        context.provider_id,
        context.caller_plugin_id,
        context.method,
    )?
    .update(request.credentials)?;
    Ok(AuthCredentialUpdateResponse {
        schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
        updated_credentials,
    })
}

/// Host security-inspection failure.
#[derive(Debug, thiserror::Error)]
pub enum AuthSecurityInspectionError {
    #[error("invalid security inspection request: {0}")]
    InvalidRequest(String),
    #[error(transparent)]
    Ownership(#[from] AuthProfileResolutionError),
}

/// Inspect security for one already-authorized auth profile.
///
/// The host derives vault location and device-seal policy from `resolved`; callers cannot supply
/// either value.
///
/// # Errors
///
/// Returns an error for invalid payloads, provider/profile mismatch, or plugin ownership mismatch.
pub fn inspect_security(
    caller_plugin_id: &str,
    resolved: &ResolvedAuthProfile,
    request: &AuthSecurityInspectionRequest,
) -> Result<AuthSecurityInspectionResponse, AuthSecurityInspectionError> {
    request
        .validate()
        .map_err(|error| AuthSecurityInspectionError::InvalidRequest(error.to_string()))?;
    validate_resolved_security_owner(caller_plugin_id, resolved, request)?;
    let vault = resolved.profile.settings.get("vault").map_or_else(
        bcode_config::default_auth_vault_path,
        std::path::PathBuf::from,
    );
    let storage_profile = resolved
        .profile
        .settings
        .get("profile")
        .map_or(resolved.profile_name.as_str(), String::as_str);
    let policy = crate::security::device_seal_policy_for_auth_profile(&resolved.profile);
    let status = crate::security::inspect_auth_vault_security(&vault, storage_profile, policy);
    Ok(AuthSecurityInspectionResponse {
        schema_version: AUTH_SECURITY_INSPECTION_SCHEMA_VERSION,
        provider_id: resolved.provider_id.clone(),
        profile: resolved.profile_name.clone(),
        policy: match policy {
            crate::security::AuthDeviceSealPolicy::Off => "off",
            crate::security::AuthDeviceSealPolicy::Preferred => "preferred",
            crate::security::AuthDeviceSealPolicy::Required => "required",
        }
        .to_owned(),
        vault_exists: status.vault_exists,
        profile_keys_enabled: status.profile_keys_enabled,
        profile_exists: status.profile_exists,
        profile_device_sealed: status.profile_device_sealed,
        policy_satisfied: status.policy_satisfied,
        diagnostics: status
            .diagnostics
            .into_iter()
            .map(|diagnostic| AuthDiagnostic {
                message: match diagnostic.code.as_str() {
                    "auth_vault_missing" => "Auth vault does not exist.".to_owned(),
                    "auth_vault_unlock_failed" => {
                        "Auth vault metadata could not be unlocked.".to_owned()
                    }
                    "auth_vault_profile_missing" => "Auth vault profile does not exist.".to_owned(),
                    "auth_vault_device_seal_missing" => {
                        "Auth vault profile is not device-sealed.".to_owned()
                    }
                    _ => "Auth security status requires attention.".to_owned(),
                },
                code: diagnostic.code,
                severity: match diagnostic.severity {
                    crate::security::AuthSecurityDiagnosticSeverity::Info => {
                        AuthDiagnosticSeverity::Info
                    }
                    crate::security::AuthSecurityDiagnosticSeverity::Warning => {
                        AuthDiagnosticSeverity::Warning
                    }
                    crate::security::AuthSecurityDiagnosticSeverity::Error => {
                        AuthDiagnosticSeverity::Error
                    }
                },
                remediation: diagnostic.remediation,
            })
            .collect(),
    })
}

fn validate_resolved_security_owner(
    caller_plugin_id: &str,
    resolved: &ResolvedAuthProfile,
    request: &AuthSecurityInspectionRequest,
) -> Result<(), AuthProfileResolutionError> {
    if request.profile != resolved.profile_name {
        return Err(AuthProfileResolutionError::MissingProfile {
            provider_id: request.provider_id.clone(),
            profile: request.profile.clone(),
        });
    }
    if request.provider_id != resolved.provider_id {
        return Err(AuthProfileResolutionError::ProviderMismatch {
            profile: resolved.profile_name.clone(),
            expected: resolved.provider_id.clone(),
            actual: request.provider_id.clone(),
        });
    }
    if caller_plugin_id != resolved.owner_plugin_id {
        return Err(AuthProfileResolutionError::OwnerMismatch {
            profile: resolved.profile_name.clone(),
            expected: resolved.owner_plugin_id.clone(),
            actual: caller_plugin_id.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_provider_auth_models::{AuthCredentialStorage, AuthMethodContribution};
    use std::collections::BTreeMap;

    fn method() -> AuthMethodContribution {
        AuthMethodContribution::Interactive {
            method_id: "chatgpt".to_owned(),
            display_name: "ChatGPT".to_owned(),
            operation: "flow".to_owned(),
            credentials: [
                ("access_token", "ACCESS_TOKEN"),
                ("refresh_token", "REFRESH_TOKEN"),
                ("expires_at", "EXPIRES_AT"),
                ("id_token", "ID_TOKEN"),
                ("account_id", "ACCOUNT_ID"),
            ]
            .into_iter()
            .map(|(credential_id, storage_key)| AuthCredentialStorage {
                credential_id: credential_id.to_owned(),
                storage_key: storage_key.to_owned(),
            })
            .collect(),
            supports_revocation: false,
        }
    }

    fn resolved(vault: &std::path::Path) -> ResolvedAuthProfile {
        ResolvedAuthProfile {
            profile_name: "openai".to_owned(),
            provider_id: "openai".to_owned(),
            owner_plugin_id: "bcode.openai-compatible".to_owned(),
            profile: bcode_config::AuthProfileConfig {
                backend: "sshenv".to_owned(),
                provider_id: Some("openai".to_owned()),
                owner_plugin_id: Some("bcode.openai-compatible".to_owned()),
                scheme: Some("chatgpt".to_owned()),
                map: BTreeMap::new(),
                settings: BTreeMap::from([
                    ("profile".to_owned(), "openai".to_owned()),
                    ("vault".to_owned(), vault.display().to_string()),
                    ("device_seal".to_owned(), "off".to_owned()),
                ]),
            },
            source: crate::AuthProfileSource::Declarative,
        }
    }

    #[test]
    fn semantic_security_inspection_derives_vault_and_denies_other_owner() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vault = temp.path().join("missing-vault");
        let mut resolved = resolved(&vault);
        resolved
            .profile
            .settings
            .insert("device_seal".to_owned(), "required".to_owned());
        let request = AuthSecurityInspectionRequest {
            schema_version: AUTH_SECURITY_INSPECTION_SCHEMA_VERSION,
            provider_id: "openai".to_owned(),
            profile: "openai".to_owned(),
        };
        let response = inspect_security("bcode.openai-compatible", &resolved, &request)
            .expect("owned inspection");
        assert_eq!(response.provider_id, "openai");
        assert_eq!(response.profile, "openai");
        assert_eq!(response.policy, "required");
        assert!(!response.vault_exists);
        assert!(!response.policy_satisfied);
        let encoded = serde_json::to_string(&response).expect("response");
        assert!(!encoded.contains(vault.to_string_lossy().as_ref()));

        assert!(matches!(
            inspect_security("bcode.other", &resolved, &request),
            Err(AuthSecurityInspectionError::Ownership(
                AuthProfileResolutionError::OwnerMismatch { .. }
            ))
        ));
        assert!(!vault.exists());
    }

    #[test]
    fn update_is_owner_bound_and_rejects_undeclared_credentials_before_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vault = temp.path().join("vault");
        let owned_resolved = resolved(&vault);
        let method = method();
        let context = AuthCredentialUpdateContext {
            caller_plugin_id: "bcode.openai-compatible",
            provider_id: "openai",
            resolved: &owned_resolved,
            method: &method,
        };
        let response = update_credentials(
            context,
            AuthCredentialUpdateRequest {
                schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
                provider_id: "openai".to_owned(),
                profile: "openai".to_owned(),
                credentials: BTreeMap::from([
                    ("access_token".to_owned(), Some("access".to_owned())),
                    ("refresh_token".to_owned(), Some("refresh".to_owned())),
                    ("expires_at".to_owned(), Some("123".to_owned())),
                ]),
            },
        )
        .expect("owned update");
        assert_eq!(
            response.updated_credentials,
            vec!["access_token", "expires_at", "refresh_token"]
        );
        let encoded = serde_json::to_string(&response).expect("response");
        assert!(!encoded.contains("\"access\""));
        assert!(!encoded.contains("\"refresh\""));
        assert!(!encoded.contains("\"123\""));

        let response = update_credentials(
            AuthCredentialUpdateContext {
                caller_plugin_id: "bcode.openai-compatible",
                provider_id: "openai",
                resolved: &owned_resolved,
                method: &method,
            },
            AuthCredentialUpdateRequest {
                schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
                provider_id: "openai".to_owned(),
                profile: "openai".to_owned(),
                credentials: BTreeMap::from([
                    ("access_token".to_owned(), Some("next-access".to_owned())),
                    ("id_token".to_owned(), None),
                    ("account_id".to_owned(), None),
                ]),
            },
        )
        .expect("atomic replacement and removal");
        assert_eq!(
            response.updated_credentials,
            vec!["access_token", "account_id", "id_token"]
        );
        let values = AuthVaultLifecycle::new(
            &owned_resolved,
            "openai",
            "bcode.openai-compatible",
            &method,
        )
        .expect("lifecycle")
        .read()
        .expect("read updated credentials");
        assert_eq!(
            values.get("access_token").map(String::as_str),
            Some("next-access")
        );
        assert!(!values.contains_key("id_token"));
        assert!(!values.contains_key("account_id"));

        let invalid_vault = temp.path().join("must-not-exist");
        let invalid_resolved = resolved(&invalid_vault);
        assert!(
            update_credentials(
                AuthCredentialUpdateContext {
                    caller_plugin_id: "bcode.other",
                    provider_id: "openai",
                    resolved: &invalid_resolved,
                    method: &method,
                },
                AuthCredentialUpdateRequest {
                    schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
                    provider_id: "openai".to_owned(),
                    profile: "openai".to_owned(),
                    credentials: BTreeMap::from([(
                        "access_token".to_owned(),
                        Some("secret".to_owned()),
                    )]),
                },
            )
            .is_err()
        );
        assert!(!invalid_vault.exists());

        let mismatched_vault = temp.path().join("provider-mismatch-must-not-exist");
        let mismatched_resolved = resolved(&mismatched_vault);
        assert!(matches!(
            update_credentials(
                AuthCredentialUpdateContext {
                    caller_plugin_id: "bcode.openai-compatible",
                    provider_id: "openai",
                    resolved: &mismatched_resolved,
                    method: &method,
                },
                AuthCredentialUpdateRequest {
                    schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
                    provider_id: "xai".to_owned(),
                    profile: "openai".to_owned(),
                    credentials: BTreeMap::from([(
                        "access_token".to_owned(),
                        Some("secret".to_owned()),
                    )]),
                },
            ),
            Err(AuthCredentialUpdateError::Ownership(
                AuthProfileResolutionError::ProviderMismatch { .. }
            ))
        ));
        assert!(!mismatched_vault.exists());

        assert!(
            update_credentials(
                AuthCredentialUpdateContext {
                    caller_plugin_id: "bcode.openai-compatible",
                    provider_id: "openai",
                    resolved: &invalid_resolved,
                    method: &method,
                },
                AuthCredentialUpdateRequest {
                    schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
                    provider_id: "openai".to_owned(),
                    profile: "openai".to_owned(),
                    credentials: BTreeMap::from([("other".to_owned(), Some("secret".to_owned()))]),
                },
            )
            .is_err()
        );
        assert!(!invalid_vault.exists());
    }
}
