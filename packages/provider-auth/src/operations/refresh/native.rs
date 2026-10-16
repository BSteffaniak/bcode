//! Durable coordination for refresh of an explicitly selected native sshenv vault.
//! The vault remains authoritative; the adjacent directory contains only ownership/fence data.

use super::{RefreshOperation, unavailable, validate_refresh_selection};
use crate::{
    ResolvedAuthProfile,
    custody_storage::{AuthCustodyStorage as _, AuthRefreshIntent, CredentialCustodyStorage},
    lifecycle::{AuthVaultLifecycle, AuthVaultLifecycleError},
};
use bcode_provider_auth_models::{
    AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION, AuthCredentialUpdateResponse, AuthMethodContribution,
    AuthRefreshBeginResponse,
};
use std::path::Path;
use zeroize::Zeroize as _;

pub fn guard(vault: &Path) -> Result<CredentialCustodyStorage, AuthVaultLifecycleError> {
    let owner = CredentialCustodyStorage::native_coordinator(vault).map_err(|_| unavailable())?;
    owner.ensure_no_refresh().map_err(|_| unavailable())?;
    Ok(owner)
}

pub fn resolve(
    resolved: &ResolvedAuthProfile,
    method: &AuthMethodContribution,
    operation: RefreshOperation,
) -> Result<serde_json::Value, AuthVaultLifecycleError> {
    let lifecycle = AuthVaultLifecycle::new(
        resolved,
        &resolved.provider_id,
        &resolved.owner_plugin_id,
        method,
    )?;
    let path = lifecycle.vault_path();
    let storage = CredentialCustodyStorage::native_coordinator(&path).map_err(|_| unavailable())?;
    let binding = super::super::provisioning_binding(resolved)?;
    match operation {
        RefreshOperation::Begin(request) => {
            request
                .validate()
                .map_err(|_| AuthVaultLifecycleError::InvalidCredential)?;
            validate_refresh_selection(&request.provider_id, &request.profile, resolved)?;
            super::super::RetainedAuthRequestCustody::validate_refresh_record(
                &lifecycle,
                &request.credentials,
            )?;
            storage.ensure_no_refresh().map_err(|_| unavailable())?;
            let mut current = lifecycle.read_uncoordinated()?;
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
            let entropy = sshenv_vault::crypto::generate_data_key();
            let intent = AuthRefreshIntent {
                version: 1,
                attempt: hex::encode(&entropy[..]),
                profile_binding: binding,
            };
            storage.begin_refresh(&intent).map_err(|_| unavailable())?;
            serde_json::to_value(AuthRefreshBeginResponse {
                schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
                attempt: intent.attempt,
            })
            .map_err(|_| unavailable())
        }
        RefreshOperation::Commit(request) => {
            request
                .validate()
                .map_err(|_| AuthVaultLifecycleError::InvalidCredential)?;
            validate_refresh_selection(&request.provider_id, &request.profile, resolved)?;
            super::super::RetainedAuthRequestCustody::validate_refresh_record(
                &lifecycle,
                &request.credentials,
            )?;
            let intent = storage.refresh_intent().map_err(|_| unavailable())?;
            if intent.attempt != request.attempt || intent.profile_binding != binding {
                return Err(AuthVaultLifecycleError::RefreshStale);
            }
            let updated_credentials = request.credentials.keys().cloned().collect();
            lifecycle.update_with(request.credentials, |changes| {
                lifecycle.persist_storage_updates_uncoordinated(changes)
            })?;
            storage.finish_refresh(&intent).map_err(|_| unavailable())?;
            serde_json::to_value(AuthCredentialUpdateResponse {
                schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
                updated_credentials,
            })
            .map_err(|_| unavailable())
        }
    }
}

pub fn read_guard(
    vault: &Path,
) -> Result<Option<CredentialCustodyStorage>, AuthVaultLifecycleError> {
    let mut name = vault.as_os_str().to_os_string();
    name.push(".refresh");
    match std::fs::symlink_metadata(std::path::PathBuf::from(name)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        _ => guard(vault).map(Some),
    }
}
