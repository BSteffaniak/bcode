//! Explicit profile-bound native operation-factor custody.

use crate::ResolvedAuthProfile;
use crate::lifecycle::AuthVaultLifecycleError;
use crate::operations::{
    AuthDeviceFactorSource, AuthProvisionedDeviceFactor, AuthProvisioningIntent,
    provisioning_binding,
};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

/// Native macOS device-only Keychain source bound to one authorized resolved profile.
///
/// Construction performs no native access. Callers authorize provisioning and reconciliation
/// before invoking custody. Operation identifiers are fresh 256-bit random values; Keychain
/// records are addressed by their profile-bound digest, never by untrusted paths.
pub struct MacosOperationFactorSource {
    profile: ResolvedAuthProfile,
    binding: String,
}

impl MacosOperationFactorSource {
    /// Bind native custody to a resolved profile without discovering configuration.
    ///
    /// # Errors
    /// Rejects a profile that cannot be encoded for ownership binding.
    pub fn new(profile: ResolvedAuthProfile) -> Result<Self, AuthVaultLifecycleError> {
        let binding = provisioning_binding(&profile)?;
        Ok(Self { profile, binding })
    }

    fn slot(&self, intent: &AuthProvisioningIntent) -> Result<String, AuthVaultLifecycleError> {
        use sha2::Digest as _;
        if intent.version != 2
            || intent.source != "macos-operation-v1"
            || intent.profile_binding != self.binding
            || intent.operation.len() != 64
            || !intent
                .operation
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(AuthVaultLifecycleError::InvalidCredential);
        }
        Ok(format!(
            "{:x}",
            sha2::Sha256::digest(
                format!("sshenv-operation-v1:{}:{}", self.binding, intent.operation).as_bytes()
            )
        ))
    }
}

impl AuthDeviceFactorSource for MacosOperationFactorSource {
    fn provisioning_identity(
        &self,
        profile: &ResolvedAuthProfile,
    ) -> Result<(String, String), AuthVaultLifecycleError> {
        use sha2::Digest as _;
        if profile != &self.profile {
            return Err(AuthVaultLifecycleError::InvalidCredential);
        }
        let random = sshenv_vault::crypto::generate_data_key();
        Ok((
            "macos-operation-v1".into(),
            format!("{:x}", sha2::Sha256::digest(&random[..])),
        ))
    }

    fn provision_attempt(
        &self,
        intent: &AuthProvisioningIntent,
    ) -> Result<AuthProvisionedDeviceFactor, AuthVaultLifecycleError> {
        let slot = self.slot(intent)?;
        let options = crate::security::device_seal_options_for_auth_profile(&self.profile.profile);
        let (factor, key) = sshenv_vault::device::create_operation_factor(&slot, options.seal)
            .map_err(|_| {
                AuthVaultLifecycleError::WriteFailed(
                    "native operation provisioning unavailable".into(),
                )
            })?;
        let mut parameters = factor.params;
        parameters.insert("bcode-profile-binding".into(), self.binding.clone());
        parameters.insert("bcode-operation".into(), intent.operation.clone());
        Ok(AuthProvisionedDeviceFactor {
            id: factor.id,
            recipient_fingerprint: factor.recipient_fingerprint,
            parameters,
            key,
        })
    }

    fn retrieve(
        &self,
        id: &str,
        recipient: Option<&str>,
        parameters: &BTreeMap<String, String>,
    ) -> Result<Zeroizing<[u8; 32]>, AuthVaultLifecycleError> {
        let intent = AuthProvisioningIntent {
            version: 2,
            source: "macos-operation-v1".into(),
            operation: parameters
                .get("bcode-operation")
                .cloned()
                .ok_or(AuthVaultLifecycleError::InvalidCredential)?,
            profile_binding: parameters
                .get("bcode-profile-binding")
                .cloned()
                .ok_or(AuthVaultLifecycleError::InvalidCredential)?,
        };
        let slot = self.slot(&intent)?;
        if recipient.is_some()
            || id != format!("device-seal-operation-{slot}")
            || parameters.get("operation") != Some(&slot)
        {
            return Err(AuthVaultLifecycleError::InvalidCredential);
        }
        let factor = sshenv_vault::models::UnlockFactorV2 {
            id: id.into(),
            kind: sshenv_vault::models::UnlockFactorKindV2::DeviceSeal,
            recipient_fingerprint: None,
            params: parameters.clone(),
        };
        let options = crate::security::device_seal_options_for_auth_profile(&self.profile.profile);
        if !sshenv_vault::device::factor_matches_options(&factor, options.seal) {
            return Err(AuthVaultLifecycleError::InvalidCredential);
        }
        sshenv_vault::device::derive_factor_from_metadata(&factor).map_err(|_| {
            AuthVaultLifecycleError::ProfileUnavailable(
                "native operation factor unavailable".into(),
            )
        })
    }

    fn reconcile_provisioning(
        &self,
        intent: &AuthProvisioningIntent,
    ) -> Result<(), AuthVaultLifecycleError> {
        let slot = self.slot(intent)?;
        sshenv_vault::device::reconcile_operation_factor(&slot)
            .map(|_| ())
            .map_err(|_| {
                AuthVaultLifecycleError::WriteFailed(
                    "native operation reconciliation unavailable".into(),
                )
            })
    }
}
