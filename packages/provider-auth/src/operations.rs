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

/// Selected host credential custody, invoked only after canonical authorization and mapping.
///
/// Implementations must atomically apply storage-key changes to the resolved profile, preserve
/// unrelated values, enforce its device-seal policy, and return secret-safe errors. This is a
/// trusted host service, never a plugin-supplied authorization hook.
pub trait AuthCredentialCustody: Send + Sync {
    /// Persist validated changes for the host-resolved owner and profile.
    ///
    /// # Errors
    /// Returns an error if ownership, policy, or atomic persistence cannot be satisfied.
    fn persist(
        &self,
        resolved: &ResolvedAuthProfile,
        changes: std::collections::BTreeMap<String, Option<String>>,
    ) -> Result<
        Vec<crate::security::AuthSecurityDiagnostic>,
        crate::lifecycle::AuthVaultLifecycleError,
    >;
}

/// Trusted request-time credential source selected by the host.
///
/// Implementations verify the destination and profile before reading secrets. Errors must be
/// secret-safe; callers must not dispatch with stale request credentials after failure.
pub trait AuthRequestCustody: Send + Sync {
    /// Materialize credentials for the selected destination and request context.
    ///
    /// # Errors
    /// Returns an error when destination ownership, selection, or custody cannot be verified.
    fn materialize(
        &self,
        plugin_id: &str,
        context: &bcode_model::ProviderRequestContext,
    ) -> Result<crate::ResolvedProviderAuth, crate::lifecycle::AuthVaultLifecycleError>;
}

/// Caller-selected retrieval of an existing device factor.
///
/// Parameters are untrusted persisted metadata, not authorization. Implementations must verify
/// factor ownership and backend policy before accessing secrets; no native fallback is implied.
pub trait AuthDeviceFactorSource: Send + Sync {
    /// Retrieve the key bound to the supplied factor metadata.
    ///
    /// # Errors
    /// Rejects unknown factors, unsupported parameters, or unverifiable device custody.
    fn retrieve(
        &self,
        id: &str,
        recipient_fingerprint: Option<&str>,
        parameters: &std::collections::BTreeMap<String, String>,
    ) -> Result<zeroize::Zeroizing<[u8; 32]>, crate::lifecycle::AuthVaultLifecycleError>;
}

/// Selected source of profile encryption keys for retained custody writes.
///
/// Production implementations must return fresh cryptographically secure keys. Deterministic
/// sources are only appropriate for isolated simulations with no real credentials.
pub trait AuthCustodyKeySource: Send + Sync {
    /// Acquire a fresh encryption key without falling back to another source.
    ///
    /// # Errors
    /// Returns a secret-safe error when key acquisition is unavailable.
    fn generate(
        &self,
    ) -> Result<zeroize::Zeroizing<[u8; 32]>, crate::lifecycle::AuthVaultLifecycleError>;
}

/// Retained encrypted custody for one explicitly bound profile.
///
/// Pools are intentionally unsupported by this single-profile service. Identities are zeroized
/// on release; directory ownership is released with storage. Device factors fail closed and
/// remote-factor limitations remain those of the lifecycle custody reader.
#[cfg(unix)]
pub struct RetainedAuthRequestCustody {
    storage: std::sync::Mutex<crate::custody_storage::CredentialCustodyStorage>,
    resolved: ResolvedAuthProfile,
    method: AuthMethodContribution,
    identities: Vec<zeroize::Zeroizing<String>>,
    passphrase: Option<zeroize::Zeroizing<String>>,
    key_source: Option<std::sync::Arc<dyn AuthCustodyKeySource>>,
    device_source: Option<std::sync::Arc<dyn AuthDeviceFactorSource>>,
}

#[cfg(unix)]
impl RetainedAuthRequestCustody {
    /// Select trusted retrieval for existing device factors on credential reads.
    ///
    /// This does not enable factor creation, remote custody, or device-protected writes.
    #[must_use]
    pub fn device_source(mut self, source: std::sync::Arc<dyn AuthDeviceFactorSource>) -> Self {
        self.device_source = Some(source);
        self
    }
    /// Select profile-key acquisition for subsequent writes, with no native fallback.
    ///
    /// The source must meet [`AuthCustodyKeySource`]'s security contract. Other custody effects
    /// are unchanged; selecting this source alone does not establish deterministic execution.
    #[must_use]
    pub fn key_source(mut self, source: std::sync::Arc<dyn AuthCustodyKeySource>) -> Self {
        self.key_source = Some(source);
        self
    }
    /// Bind retained storage and identities to a registered provider/profile owner.
    ///
    /// # Errors
    /// Rejects mismatched provider, plugin, backend, or method ownership.
    pub fn new(
        storage: crate::custody_storage::CredentialCustodyStorage,
        resolved: ResolvedAuthProfile,
        provider_id: &str,
        plugin_id: &str,
        method: AuthMethodContribution,
        identities: Vec<zeroize::Zeroizing<String>>,
        passphrase: Option<zeroize::Zeroizing<String>>,
    ) -> Result<Self, crate::lifecycle::AuthVaultLifecycleError> {
        AuthVaultLifecycle::new(&resolved, provider_id, plugin_id, &method)?;
        Ok(Self {
            storage: std::sync::Mutex::new(storage),
            resolved,
            method,
            identities,
            passphrase,
            key_source: None,
            device_source: None,
        })
    }
}

#[cfg(unix)]
impl AuthRequestCustody for RetainedAuthRequestCustody {
    fn materialize(
        &self,
        plugin_id: &str,
        context: &bcode_model::ProviderRequestContext,
    ) -> Result<crate::ResolvedProviderAuth, crate::lifecycle::AuthVaultLifecycleError> {
        if plugin_id != self.resolved.owner_plugin_id
            || context.auth_profile.as_deref() != Some(self.resolved.profile_name.as_str())
            || context.auth_pool.is_some()
            || !context.auth_candidates.is_empty()
        {
            return Err(
                crate::lifecycle::AuthVaultLifecycleError::ProfileUnavailable(
                    "request does not match retained credential ownership".into(),
                ),
            );
        }
        let lifecycle = AuthVaultLifecycle::new(
            &self.resolved,
            &self.resolved.provider_id,
            plugin_id,
            &self.method,
        )?;
        let identities = self
            .identities
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>();
        let storage = self.storage.lock().map_err(|_| {
            crate::lifecycle::AuthVaultLifecycleError::VaultUnavailable(
                "custody owner unavailable".into(),
            )
        })?;
        lifecycle.materialize_from_custody_with_device(
            &storage,
            &identities,
            self.passphrase.as_ref().map(|value| value.as_str()),
            self.device_source.as_deref(),
        )
    }
}

#[cfg(unix)]
impl AuthCredentialCustody for RetainedAuthRequestCustody {
    fn persist(
        &self,
        resolved: &ResolvedAuthProfile,
        changes: std::collections::BTreeMap<String, Option<String>>,
    ) -> Result<
        Vec<crate::security::AuthSecurityDiagnostic>,
        crate::lifecycle::AuthVaultLifecycleError,
    > {
        if resolved.profile_name != self.resolved.profile_name
            || resolved.provider_id != self.resolved.provider_id
            || resolved.owner_plugin_id != self.resolved.owner_plugin_id
            || resolved.profile != self.resolved.profile
        {
            return Err(crate::lifecycle::AuthVaultLifecycleError::InvalidCredential);
        }
        let lifecycle = AuthVaultLifecycle::new(
            &self.resolved,
            &self.resolved.provider_id,
            &self.resolved.owner_plugin_id,
            &self.method,
        )?;
        let identities = self
            .identities
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>();
        let mut storage = self.storage.lock().map_err(|_| {
            crate::lifecycle::AuthVaultLifecycleError::VaultUnavailable(
                "custody owner unavailable".into(),
            )
        })?;
        lifecycle.persist_to_custody(
            &mut storage,
            &identities,
            self.passphrase.as_ref().map(|value| value.as_str()),
            changes,
            self.key_source.as_deref(),
        )
    }
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
    update_credentials_with_custody(context, request, None)
}

/// Update credentials using canonical authorization and optionally selected host custody.
///
/// # Errors
/// Returns payload, ownership, credential-shape, or custody errors before reporting success.
pub fn update_credentials_with_custody(
    context: AuthCredentialUpdateContext<'_>,
    request: AuthCredentialUpdateRequest,
    custody: Option<&dyn AuthCredentialCustody>,
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
    let lifecycle = AuthVaultLifecycle::new(
        context.resolved,
        context.provider_id,
        context.caller_plugin_id,
        context.method,
    )?;
    if let Some(custody) = custody {
        lifecycle.update_with(request.credentials, |changes| {
            custody.persist(context.resolved, changes)
        })?;
    } else {
        lifecycle.update(request.credentials)?;
    }
    Ok(AuthCredentialUpdateResponse {
        schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
        updated_credentials,
    })
}

/// Registered auth-provider identity bound by the host from its plugin registry.
///
/// The host looks this up by the requested provider ID; the caller cannot supply it.
#[derive(Debug, Clone, Copy)]
pub struct RegisteredAuthProviderOwner<'a> {
    /// Plugin that registered the provider contribution.
    pub plugin_id: &'a str,
    /// Methods the registered contribution declares.
    pub methods: &'a [AuthMethodContribution],
}

/// Host-owned resolution for one `bcode.provider-auth-host` bridge request.
///
/// Both the daemon and the embedded runtime route plugin-initiated
/// [`bcode_provider_auth_models::OP_UPDATE_CREDENTIALS`] requests through this function so the
/// ownership rules stay identical across hosts:
///
/// * The caller plugin must be the plugin that registered the target auth provider.
/// * The requested profile must resolve for that provider and owner.
/// * The resolved profile's scheme must name a method the registration declares.
/// * Vault custody and credential-shape checks are enforced by [`update_credentials`].
///
/// Requests for other interfaces or operations return
/// [`bcode_tool::ToolInvocationServiceResolution::Unsupported`] so hosts can chain resolvers.
#[must_use]
pub fn resolve_credential_update_service_request<'registry>(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    caller_plugin_id: &str,
    registered_provider: impl FnOnce(&str) -> Option<RegisteredAuthProviderOwner<'registry>>,
    request: bcode_tool::ToolInvocationServiceRequest,
) -> bcode_tool::ToolInvocationServiceResolution {
    resolve_credential_update_service_request_with_custody(
        config,
        runtime,
        caller_plugin_id,
        registered_provider,
        request,
        None,
    )
}

/// Resolve a plugin credential update with optionally selected trusted host custody.
///
/// Uses the same registry ownership, profile resolution, and credential validation as the
/// native entry point; custody cannot override authorization.
#[must_use]
pub fn resolve_credential_update_service_request_with_custody<'registry>(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    caller_plugin_id: &str,
    registered_provider: impl FnOnce(&str) -> Option<RegisteredAuthProviderOwner<'registry>>,
    request: bcode_tool::ToolInvocationServiceRequest,
    custody: Option<&dyn AuthCredentialCustody>,
) -> bcode_tool::ToolInvocationServiceResolution {
    use bcode_provider_auth_models::{AUTH_HOST_INTERFACE_ID, OP_UPDATE_CREDENTIALS};
    use bcode_tool::ToolInvocationServiceResolution as Resolution;

    if request.interface_id != AUTH_HOST_INTERFACE_ID || request.operation != OP_UPDATE_CREDENTIALS
    {
        return Resolution::Unsupported;
    }
    let Ok(update) = serde_json::from_value::<AuthCredentialUpdateRequest>(request.payload) else {
        return Resolution::Failed {
            code: "invalid_request".to_owned(),
            message: "invalid provider-auth credential update request".to_owned(),
        };
    };
    let provider_id = update.provider_id.clone();
    let Some(registered) = registered_provider(&provider_id) else {
        return Resolution::Failed {
            code: "auth_provider_unregistered".to_owned(),
            message: "authentication provider is not registered".to_owned(),
        };
    };
    if registered.plugin_id != caller_plugin_id {
        return Resolution::Failed {
            code: "auth_owner_mismatch".to_owned(),
            message: "authentication provider is owned by another plugin".to_owned(),
        };
    }
    let resolved = match crate::resolve_auth_provider_profile(
        config,
        &provider_id,
        caller_plugin_id,
        Some(&update.profile),
        runtime,
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            return Resolution::Failed {
                code: "auth_profile_unavailable".to_owned(),
                message: error.to_string(),
            };
        }
    };
    let Some(method) = registered
        .methods
        .iter()
        .find(|method| resolved.profile.scheme.as_deref() == Some(method.method_id()))
    else {
        return Resolution::Failed {
            code: "auth_method_unavailable".to_owned(),
            message: "owned auth profile method is not registered".to_owned(),
        };
    };
    match update_credentials_with_custody(
        AuthCredentialUpdateContext {
            caller_plugin_id,
            provider_id: &provider_id,
            resolved: &resolved,
            method,
        },
        update,
        custody,
    ) {
        Ok(response) => serde_json::to_value(response).map_or_else(
            |_| Resolution::Failed {
                code: "auth_response_encode_failed".to_owned(),
                message: "credential update response could not be encoded".to_owned(),
            },
            |payload| Resolution::Responded { payload },
        ),
        Err(error) => Resolution::Failed {
            code: "auth_credential_update_failed".to_owned(),
            message: error.to_string(),
        },
    }
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
                map: [
                    ("access_token", "ACCESS_TOKEN"),
                    ("refresh_token", "REFRESH_TOKEN"),
                    ("expires_at", "EXPIRES_AT"),
                    ("id_token", "ID_TOKEN"),
                    ("account_id", "ACCOUNT_ID"),
                ]
                .into_iter()
                .map(|(credential, key)| {
                    (
                        credential.to_owned(),
                        bcode_config::AuthCredentialMapping {
                            env: None,
                            key: Some(key.to_owned()),
                        },
                    )
                })
                .collect(),
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
    fn selected_custody_cannot_bypass_owner_or_credential_validation() {
        struct UnavailableCustody(std::sync::atomic::AtomicUsize);
        impl AuthCredentialCustody for UnavailableCustody {
            fn persist(
                &self,
                resolved: &ResolvedAuthProfile,
                changes: BTreeMap<String, Option<String>>,
            ) -> Result<
                Vec<crate::security::AuthSecurityDiagnostic>,
                crate::lifecycle::AuthVaultLifecycleError,
            > {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                assert_eq!(resolved.profile_name, "openai");
                assert_eq!(changes.len(), 1);
                Err(crate::lifecycle::AuthVaultLifecycleError::WriteFailed(
                    "custody unavailable".into(),
                ))
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let vault = temp.path().join("unused");
        let resolved = resolved(&vault);
        let method = method();
        let custody = UnavailableCustody(std::sync::atomic::AtomicUsize::new(0));
        for (owner, credential) in [
            ("other", "access_token"),
            ("bcode.openai-compatible", "unknown"),
            ("bcode.openai-compatible", "access_token"),
        ] {
            let result = update_credentials_with_custody(
                AuthCredentialUpdateContext {
                    caller_plugin_id: owner,
                    provider_id: "openai",
                    resolved: &resolved,
                    method: &method,
                },
                AuthCredentialUpdateRequest {
                    schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
                    provider_id: "openai".into(),
                    profile: "openai".into(),
                    credentials: BTreeMap::from([(credential.into(), Some("value".into()))]),
                },
                Some(&custody),
            );
            assert!(result.is_err());
        }
        assert_eq!(custody.0.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(!vault.exists());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
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
        let rematerialized = crate::resolve_auth_profile("openai", &owned_resolved.profile);
        assert_eq!(
            rematerialized
                .auth
                .credentials
                .get("access_token")
                .map(|credential| credential.value.as_str()),
            Some("next-access")
        );
        assert!(!rematerialized.auth.credentials.contains_key("id_token"));
        assert!(!rematerialized.auth.credentials.contains_key("account_id"));

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

    fn update_service_request(
        provider_id: &str,
        profile: &str,
        access_token: &str,
    ) -> bcode_tool::ToolInvocationServiceRequest {
        bcode_tool::ToolInvocationServiceRequest {
            invocation_id: "turn-1".to_owned(),
            request_id: "turn-1-credential-refresh".to_owned(),
            route_id: None,
            interface_id: bcode_provider_auth_models::AUTH_HOST_INTERFACE_ID.to_owned(),
            operation: bcode_provider_auth_models::OP_UPDATE_CREDENTIALS.to_owned(),
            payload: serde_json::to_value(AuthCredentialUpdateRequest {
                schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
                provider_id: provider_id.to_owned(),
                profile: profile.to_owned(),
                credentials: BTreeMap::from([
                    ("access_token".to_owned(), Some(access_token.to_owned())),
                    (
                        "refresh_token".to_owned(),
                        Some("rotated-refresh".to_owned()),
                    ),
                    ("expires_at".to_owned(), Some("4102444800".to_owned())),
                    ("id_token".to_owned(), None),
                    ("account_id".to_owned(), None),
                ]),
            })
            .expect("update payload"),
        }
    }

    fn failed_code(resolution: &bcode_tool::ToolInvocationServiceResolution) -> &str {
        match resolution {
            bcode_tool::ToolInvocationServiceResolution::Failed { code, .. } => code,
            other => panic!("expected failed resolution, got {other:?}"),
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn credential_update_service_request_binds_registry_ownership_and_persists_to_vault() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vault = temp.path().join("vault");
        let resolved = resolved(&vault);
        let method = method();
        let methods = vec![method.clone()];
        let config = bcode_config::BcodeConfig {
            auth: bcode_config::AuthConfig {
                profiles: BTreeMap::from([("openai".to_owned(), resolved.profile.clone())]),
                ..bcode_config::AuthConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        let runtime = bcode_config::RuntimeAuthSubscriptions::default();
        let registry = |plugin_id: &'static str| {
            let registered = BTreeMap::from([(
                resolved.provider_id.clone(),
                RegisteredAuthProviderOwner {
                    plugin_id,
                    methods: &methods,
                },
            )]);
            move |provider_id: &str| registered.get(provider_id).copied()
        };

        // Foreign interface/operation: hosts must be able to chain resolvers.
        let mut unrelated = update_service_request("openai", "openai", "ignored");
        unrelated.operation = "inspect_security".to_owned();
        assert!(matches!(
            resolve_credential_update_service_request(
                &config,
                &runtime,
                "bcode.openai-compatible",
                registry("bcode.openai-compatible"),
                unrelated,
            ),
            bcode_tool::ToolInvocationServiceResolution::Unsupported
        ));

        // Unregistered provider IDs fail before any profile lookup.
        assert_eq!(
            failed_code(&resolve_credential_update_service_request(
                &config,
                &runtime,
                "bcode.openai-compatible",
                registry("bcode.openai-compatible"),
                update_service_request("xai", "openai", "leak"),
            )),
            "auth_provider_unregistered"
        );

        // The caller must be the plugin that registered the provider.
        assert_eq!(
            failed_code(&resolve_credential_update_service_request(
                &config,
                &runtime,
                "bcode.other",
                registry("bcode.openai-compatible"),
                update_service_request("openai", "openai", "leak"),
            )),
            "auth_owner_mismatch"
        );

        // The profile must resolve for the provider and owner.
        assert_eq!(
            failed_code(&resolve_credential_update_service_request(
                &config,
                &runtime,
                "bcode.openai-compatible",
                registry("bcode.openai-compatible"),
                update_service_request("openai", "missing-profile", "leak"),
            )),
            "auth_profile_unavailable"
        );

        // The resolved profile scheme must name a registered method.
        let api_key_only = vec![AuthMethodContribution::SecretFields {
            method_id: "api_key".to_owned(),
            display_name: "API key".to_owned(),
            fields: Vec::new(),
            supports_verification: false,
            supports_revocation: false,
        }];
        assert_eq!(
            failed_code(&resolve_credential_update_service_request(
                &config,
                &runtime,
                "bcode.openai-compatible",
                |_: &str| Some(RegisteredAuthProviderOwner {
                    plugin_id: "bcode.openai-compatible",
                    methods: &api_key_only,
                }),
                update_service_request("openai", "openai", "leak"),
            )),
            "auth_method_unavailable"
        );
        assert!(
            !vault.exists(),
            "rejected requests must not touch the vault"
        );

        // Owned, well-formed request persists through host custody.
        let resolution = resolve_credential_update_service_request(
            &config,
            &runtime,
            "bcode.openai-compatible",
            registry("bcode.openai-compatible"),
            update_service_request("openai", "openai", "fresh-access"),
        );
        let bcode_tool::ToolInvocationServiceResolution::Responded { payload } = resolution else {
            panic!("expected responded resolution, got {resolution:?}");
        };
        let response: AuthCredentialUpdateResponse =
            serde_json::from_value(payload).expect("update response");
        assert_eq!(
            response.updated_credentials,
            vec![
                "access_token",
                "account_id",
                "expires_at",
                "id_token",
                "refresh_token",
            ]
        );
        let stored =
            AuthVaultLifecycle::new(&resolved, "openai", "bcode.openai-compatible", &method)
                .expect("owned lifecycle")
                .read()
                .expect("read vault");
        assert_eq!(
            stored.get("access_token").map(String::as_str),
            Some("fresh-access")
        );
        assert_eq!(
            stored.get("refresh_token").map(String::as_str),
            Some("rotated-refresh")
        );
        assert_eq!(
            stored.get("expires_at").map(String::as_str),
            Some("4102444800")
        );
        assert!(!stored.contains_key("id_token"));
    }
}
