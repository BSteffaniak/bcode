//! Single-profile durable refresh admission and host service routing.

use super::{
    AuthCredentialCustody, AuthVaultLifecycle, RegisteredAuthProviderOwner,
    RetainedAuthRequestCustody,
};
use crate::{
    ResolvedAuthProfile,
    custody_storage::{AuthCustodyStorage, AuthRefreshIntent, CustodyStorageError},
    lifecycle::AuthVaultLifecycleError,
};
use bcode_provider_auth_models::{
    AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION, AUTH_HOST_INTERFACE_ID, AuthCredentialUpdateResponse,
    AuthRefreshBeginRequest, AuthRefreshBeginResponse, AuthRefreshCommitRequest, OP_BEGIN_REFRESH,
    OP_COMMIT_REFRESH,
};
use std::collections::{BTreeMap, BTreeSet};
use zeroize::Zeroize as _;

pub mod native;

const fn unavailable() -> AuthVaultLifecycleError {
    AuthVaultLifecycleError::RefreshBlocked
}

fn validate_refresh_selection(
    provider_id: &str,
    profile_name: &str,
    resolved: &ResolvedAuthProfile,
) -> Result<(), AuthVaultLifecycleError> {
    if provider_id != resolved.provider_id {
        return Err(AuthVaultLifecycleError::InvalidCredential);
    }
    if profile_name != resolved.profile_name {
        return Err(AuthVaultLifecycleError::InvalidCredential);
    }
    Ok(())
}

pub enum RefreshOperation {
    Begin(AuthRefreshBeginRequest),
    Commit(AuthRefreshCommitRequest),
}

impl RetainedAuthRequestCustody {
    pub(super) fn refresh_lifecycle(
        &self,
        resolved: &ResolvedAuthProfile,
    ) -> Result<AuthVaultLifecycle<'_>, AuthVaultLifecycleError> {
        if resolved.profile_name != self.resolved.profile_name
            || resolved.provider_id != self.resolved.provider_id
            || resolved.owner_plugin_id != self.resolved.owner_plugin_id
            || resolved.profile != self.resolved.profile
            || resolved.source != self.resolved.source
        {
            return Err(AuthVaultLifecycleError::InvalidCredential);
        }
        AuthVaultLifecycle::new(
            &self.resolved,
            &self.resolved.provider_id,
            &self.resolved.owner_plugin_id,
            &self.method,
        )
    }

    fn validate_refresh_record(
        lifecycle: &AuthVaultLifecycle<'_>,
        record: &BTreeMap<String, Option<String>>,
    ) -> Result<(), AuthVaultLifecycleError> {
        let keys = lifecycle.credential_storage_keys()?;
        if keys.keys().collect::<BTreeSet<_>>() != record.keys().collect::<BTreeSet<_>>()
            || keys.values().collect::<BTreeSet<_>>().len() != keys.len()
        {
            return Err(AuthVaultLifecycleError::InvalidCredential);
        }
        Ok(())
    }

    pub(super) fn begin_owned_refresh(
        &self,
        resolved: &ResolvedAuthProfile,
        request: &AuthRefreshBeginRequest,
    ) -> Result<AuthRefreshBeginResponse, AuthVaultLifecycleError> {
        request
            .validate()
            .map_err(|_| AuthVaultLifecycleError::InvalidCredential)?;
        validate_refresh_selection(&request.provider_id, &request.profile, resolved)?;
        let lifecycle = self.refresh_lifecycle(resolved)?;
        Self::validate_refresh_record(&lifecycle, &request.credentials)?;
        let storage = self.storage.lock().map_err(|_| unavailable())?;
        let mut active = self.active_refresh.lock().map_err(|_| unavailable())?;
        if active.is_some() {
            return Err(unavailable());
        }
        storage.ensure_no_refresh().map_err(|_| unavailable())?;
        storage
            .ensure_no_provisioning()
            .map_err(|_| unavailable())?;
        let identities = self
            .identities
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>();
        let mut current = lifecycle.read_from_custody_with_device(
            storage.as_ref(),
            &identities,
            self.passphrase.as_ref().map(|value| value.as_str()),
            self.device_source.as_deref(),
        )?;
        let matches = request
            .credentials
            .iter()
            .all(|(id, value)| current.get(id) == value.as_ref());
        for value in current.values_mut() {
            value.zeroize();
        }
        if !matches {
            return Err(AuthVaultLifecycleError::RefreshStale);
        }
        // A fresh host-owned random identity is independent of credentials and process lifetime.
        let entropy = sshenv_vault::crypto::generate_data_key();
        let intent = AuthRefreshIntent {
            version: 1,
            attempt: hex::encode(&entropy[..]),
            profile_binding: super::provisioning_binding(resolved)?,
        };
        storage.begin_refresh(&intent).map_err(|_| unavailable())?;
        *active = Some(intent.attempt.clone());
        drop(active);
        drop(storage);
        Ok(AuthRefreshBeginResponse {
            schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
            attempt: intent.attempt,
        })
    }

    pub(super) fn commit_owned_refresh(
        &self,
        resolved: &ResolvedAuthProfile,
        request: AuthRefreshCommitRequest,
    ) -> Result<AuthCredentialUpdateResponse, AuthVaultLifecycleError> {
        request
            .validate()
            .map_err(|_| AuthVaultLifecycleError::InvalidCredential)?;
        validate_refresh_selection(&request.provider_id, &request.profile, resolved)?;
        let lifecycle = self.refresh_lifecycle(resolved)?;
        Self::validate_refresh_record(&lifecycle, &request.credentials)?;
        let mut storage = self.storage.lock().map_err(|_| unavailable())?;
        let mut active = self.active_refresh.lock().map_err(|_| unavailable())?;
        if active.as_deref() != Some(request.attempt.as_str()) {
            return Err(unavailable());
        }
        let intent = storage.refresh_intent().map_err(|_| unavailable())?;
        if intent.attempt != request.attempt
            || intent.profile_binding != super::provisioning_binding(resolved)?
        {
            return Err(AuthVaultLifecycleError::InvalidCredential);
        }
        // Commit is single-use even if validation/publication subsequently fails. Restarted
        // owners cannot revive an old remote attempt from the durable fence alone.
        *active = None;
        drop(active);
        self.publish_refresh_record(&lifecycle, storage.as_mut(), &intent, request.credentials)
    }

    /// Publish a freshly enrolled complete record for this exact owner and clear its refresh fence.
    /// This is a trusted host maintenance operation, not a plugin service or implicit retry.
    /// The host must authorize explicit re-enrollment and settle/cancel outstanding invocation work
    /// before calling; an old remote exchange must never be interpreted as the fresh enrollment.
    ///
    /// # Errors
    /// Foreign profiles, incomplete records, unresolved device provisioning, and uncertain writes
    /// preserve the fence. Interrupted rotation is not recovered by replaying the remote exchange.
    pub fn reenroll_after_refresh(
        &self,
        resolved: &ResolvedAuthProfile,
        credentials: BTreeMap<String, Option<String>>,
    ) -> Result<AuthCredentialUpdateResponse, AuthVaultLifecycleError> {
        let lifecycle = self.refresh_lifecycle(resolved)?;
        bcode_provider_auth_models::AuthCredentialUpdateRequest {
            schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
            provider_id: resolved.provider_id.clone(),
            profile: resolved.profile_name.clone(),
            credentials: credentials.clone(),
        }
        .validate()
        .map_err(|_| AuthVaultLifecycleError::InvalidCredential)?;
        Self::validate_refresh_record(&lifecycle, &credentials)?;
        let mut storage = self.storage.lock().map_err(|_| unavailable())?;
        let intent = storage.refresh_intent().map_err(|_| unavailable())?;
        if intent.profile_binding != super::provisioning_binding(resolved)? {
            return Err(AuthVaultLifecycleError::InvalidCredential);
        }
        *self.active_refresh.lock().map_err(|_| unavailable())? = None;
        *self.active_enrollment.lock().map_err(|_| unavailable())? = None;
        self.publish_refresh_record(&lifecycle, storage.as_mut(), &intent, credentials)
    }

    pub(super) fn publish_refresh_record(
        &self,
        lifecycle: &AuthVaultLifecycle<'_>,
        storage: &mut dyn AuthCustodyStorage,
        intent: &AuthRefreshIntent,
        credentials: BTreeMap<String, Option<String>>,
    ) -> Result<AuthCredentialUpdateResponse, AuthVaultLifecycleError> {
        let updated_credentials = credentials.keys().cloned().collect();
        let identities = self
            .identities
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>();
        // Only this validated commit bypasses the refresh admission guard. Device provisioning
        // and vault policy still run unchanged, and the fence remains until durable publication.
        lifecycle.update_with(credentials, |changes| {
            lifecycle.persist_to_custody(
                &mut RefreshCommitStorage(storage),
                &identities,
                self.passphrase.as_ref().map(|value| value.as_str()),
                changes,
                self.key_source.as_deref(),
                self.device_source.as_deref(),
            )
        })?;
        storage.finish_refresh(intent).map_err(|_| unavailable())?;
        Ok(AuthCredentialUpdateResponse {
            schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
            updated_credentials,
        })
    }
}

struct RefreshCommitStorage<'a>(&'a mut dyn AuthCustodyStorage);
impl AuthCustodyStorage for RefreshCommitStorage<'_> {
    fn read(&self) -> Result<Vec<u8>, CustodyStorageError> {
        self.0.read()
    }
    fn compare_and_publish(
        &mut self,
        expected: &[u8],
        ciphertext: &[u8],
    ) -> Result<(), CustodyStorageError> {
        self.0.compare_and_publish(expected, ciphertext)
    }
    fn ensure_no_refresh(&self) -> Result<(), CustodyStorageError> {
        Ok(())
    }
    fn ensure_no_provisioning(&self) -> Result<(), CustodyStorageError> {
        self.0.ensure_no_provisioning()
    }
    fn begin_provisioning(
        &self,
        intent: &super::AuthProvisioningIntent,
    ) -> Result<(), CustodyStorageError> {
        self.0.begin_provisioning(intent)
    }
    fn provisioning_intent(&self) -> Result<super::AuthProvisioningIntent, CustodyStorageError> {
        self.0.provisioning_intent()
    }
    fn finish_provisioning(&self) -> Result<(), CustodyStorageError> {
        self.0.finish_provisioning()
    }
}

/// Route typed refresh operations using canonical plugin/provider/profile authorization.
///
/// Pass the same retained custody used for request materialization and credential updates.
/// No custody means unsupported coordination, never an unfenced native-vault fallback.
#[must_use]
pub fn resolve_refresh_service_request_with_custody<'registry>(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    caller_plugin_id: &str,
    registered_provider: impl FnOnce(&str) -> Option<RegisteredAuthProviderOwner<'registry>>,
    request: bcode_tool::ToolInvocationServiceRequest,
    custody: Option<&dyn AuthCredentialCustody>,
) -> bcode_tool::ToolInvocationServiceResolution {
    use bcode_tool::ToolInvocationServiceResolution as Resolution;
    if request.interface_id != AUTH_HOST_INTERFACE_ID
        || !matches!(
            request.operation.as_str(),
            OP_BEGIN_REFRESH | OP_COMMIT_REFRESH
        )
    {
        return Resolution::Unsupported;
    }
    let operation = if request.operation == OP_BEGIN_REFRESH {
        serde_json::from_value::<AuthRefreshBeginRequest>(request.payload)
            .ok()
            .filter(|value| value.validate().is_ok())
            .map(RefreshOperation::Begin)
    } else {
        serde_json::from_value::<AuthRefreshCommitRequest>(request.payload)
            .ok()
            .filter(|value| value.validate().is_ok())
            .map(RefreshOperation::Commit)
    };
    let Some(operation) = operation else {
        return Resolution::Failed {
            code: "invalid_request".into(),
            message: "invalid credential refresh request".into(),
        };
    };
    let (provider_id, profile) = match &operation {
        RefreshOperation::Begin(value) => (&value.provider_id, &value.profile),
        RefreshOperation::Commit(value) => (&value.provider_id, &value.profile),
    };
    let Some(registered) = registered_provider(provider_id) else {
        return Resolution::Failed {
            code: "auth_provider_unregistered".into(),
            message: "authentication provider is not registered".into(),
        };
    };
    if registered.plugin_id != caller_plugin_id {
        return Resolution::Failed {
            code: "auth_owner_mismatch".into(),
            message: "authentication provider is owned by another plugin".into(),
        };
    }
    let Ok(resolved) = crate::resolve_auth_provider_profile(
        config,
        provider_id,
        caller_plugin_id,
        Some(profile),
        runtime,
    ) else {
        return Resolution::Failed {
            code: "auth_profile_unavailable".into(),
            message: "credential refresh profile is unavailable".into(),
        };
    };
    let Some(method) = registered
        .methods
        .iter()
        .find(|method| Some(method.method_id()) == resolved.profile.scheme.as_deref())
    else {
        return Resolution::Failed {
            code: "auth_profile_unavailable".into(),
            message: "credential refresh method is unavailable".into(),
        };
    };
    let result = if let Some(custody) = custody {
        match operation {
            RefreshOperation::Begin(value) => custody
                .begin_refresh(&resolved, value)
                .and_then(|value| serde_json::to_value(value).map_err(|_| unavailable())),
            RefreshOperation::Commit(value) => custody
                .commit_refresh(&resolved, value)
                .and_then(|value| serde_json::to_value(value).map_err(|_| unavailable())),
        }
    } else {
        native::resolve(&resolved, method, operation)
    };
    match result {
        Ok(payload) => Resolution::Responded { payload },
        Err(AuthVaultLifecycleError::RefreshUnsupported) => Resolution::Failed { code: "auth_refresh_unavailable".into(), message: "durable refresh coordination is unavailable".into() },
        Err(AuthVaultLifecycleError::RefreshStale) => Resolution::Failed { code: "auth_refresh_stale".into(), message: "credential snapshot changed; reload before retrying".into() },
        Err(_) => Resolution::Failed { code: "auth_refresh_blocked".into(), message: "credential refresh could not be admitted or committed; reload or re-enroll the owned profile".into() },
    }
}
