//! Bcode auth vault security policy.
//!
//! The vault identity, device-seal reconciliation, and inspection mechanics live upstream in
//! [`sshenv_vault::app`]. This module binds them to Bcode's managed key name, parses Bcode auth
//! profile settings, and maps upstream outcomes onto Bcode's stable diagnostics.

use bcode_plugin_sdk::path::display_from_current_dir;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sshenv_vault::app::{
    AppVault, AppVaultConfig, AppVaultError, SecurityAction, SecurityIssue, SecurityReport,
    profile_device_seal_matches, profile_has_device_seal, zeroize_vault_values,
};
use sshenv_vault::models::VERSION_V2;

pub use sshenv_vault::app::{
    AppDeviceSealOptions as AuthDeviceSealOptions, AppDeviceSealPolicy as AuthDeviceSealPolicy,
};

/// Managed key name: keys live at `<vault>.keys/bcode_sshenv_ed25519{,.pub}`.
const BCODE_VAULT_KEY_NAME: &str = "bcode";

fn auth_vault(vault_path: &Path) -> AppVault {
    AppVault::new(AppVaultConfig::new(BCODE_VAULT_KEY_NAME, vault_path))
}

fn auth_vault_with_options(vault_path: &Path, options: AuthDeviceSealOptions) -> AppVault {
    AppVault::new(AppVaultConfig::new(BCODE_VAULT_KEY_NAME, vault_path).with_device_seal(options))
}

/// Error returned when Bcode-managed auth vault identity material cannot be used.
#[derive(Debug)]
pub enum AuthIdentityError {
    /// Identity material exists only partially and must be repaired manually.
    IncompleteIdentity {
        private_key: PathBuf,
        public_key: PathBuf,
    },
    /// Filesystem or SSH key operation failed.
    OperationFailed { message: String },
}

impl fmt::Display for AuthIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncompleteIdentity {
                private_key,
                public_key,
            } => write!(
                formatter,
                "Bcode-managed auth vault identity is incomplete; expected both {} and {}. Remove the incomplete key files and run `bcode login` again.",
                display_from_current_dir(private_key),
                display_from_current_dir(public_key)
            ),
            Self::OperationFailed { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AuthIdentityError {}

impl From<AppVaultError> for AuthIdentityError {
    fn from(error: AppVaultError) -> Self {
        match error {
            AppVaultError::IncompleteIdentity {
                private_key,
                public_key,
            } => Self::IncompleteIdentity {
                private_key,
                public_key,
            },
            other => Self::OperationFailed {
                message: format!("Bcode auth {other}"),
            },
        }
    }
}

/// Return the Bcode-managed key directory for an sshenv vault path.
#[must_use]
pub fn vault_identity_dir(vault_path: &Path) -> PathBuf {
    auth_vault(vault_path).identity_dir()
}

/// Return the Bcode-managed private key path for an sshenv vault path.
#[must_use]
pub fn vault_private_key_path(vault_path: &Path) -> PathBuf {
    auth_vault(vault_path).private_key_path()
}

/// Return the Bcode-managed public key path for an sshenv vault path.
#[must_use]
pub fn vault_public_key_path(vault_path: &Path) -> PathBuf {
    auth_vault(vault_path).public_key_path()
}

/// Return private-key paths Bcode may use to unlock an auth vault.
#[must_use]
pub fn vault_private_key_paths(vault_path: &Path) -> Vec<PathBuf> {
    auth_vault(vault_path).private_key_paths()
}

/// Return the sshenv store for an auth vault, unlocking only with the Bcode-managed key.
#[must_use]
pub fn auth_vault_store(vault_path: &Path) -> sshenv_vault::SshenvStore {
    auth_vault(vault_path).store()
}

/// Read the Bcode-managed auth vault recipient key if the complete keypair exists.
///
/// # Errors
///
/// * Only part of the managed keypair exists
/// * The public key cannot be read
pub fn read_vault_recipient_key(vault_path: &Path) -> Result<Option<String>, AuthIdentityError> {
    Ok(auth_vault(vault_path).read_recipient_key()?)
}

/// Create or reuse the Bcode-managed auth vault recipient key for a vault.
///
/// # Errors
///
/// * Only part of the managed keypair exists
/// * Key generation, filesystem writes, or reading the resulting public key fails
pub fn ensure_vault_recipient_key(vault_path: &Path) -> Result<String, AuthIdentityError> {
    Ok(auth_vault(vault_path).ensure_recipient_key()?)
}

/// Validate an existing auth vault, or create it with the Bcode-managed identity, sshenv v2,
/// and per-profile encryption. Damaged vaults are never reset. Returns the recipient key.
///
/// # Errors
///
/// * Identity errors from [`ensure_vault_recipient_key`]
/// * An existing vault cannot be unlocked
/// * A new vault cannot be written
pub(crate) fn open_or_create_auth_vault(vault_path: &Path) -> Result<String, AppVaultError> {
    auth_vault(vault_path).open_or_create()
}

/// Severity for auth vault security diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthSecurityDiagnosticSeverity {
    /// Informational diagnostic.
    Info,
    /// Warning diagnostic; auth can continue but security policy is not fully satisfied.
    Warning,
    /// Error diagnostic; auth cannot satisfy required security policy.
    Error,
}

impl AuthSecurityDiagnosticSeverity {
    /// String label for this severity.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// Structured auth vault security diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSecurityDiagnostic {
    /// Severity level.
    pub severity: AuthSecurityDiagnosticSeverity,
    /// Stable diagnostic code.
    pub code: String,
    /// Human-readable diagnostic message.
    pub message: String,
    /// Optional remediation guidance.
    #[serde(default)]
    pub remediation: Option<String>,
}

impl AuthSecurityDiagnostic {
    #[must_use]
    pub(crate) fn info(code: &str, message: impl Into<String>) -> Self {
        Self {
            severity: AuthSecurityDiagnosticSeverity::Info,
            code: code.to_string(),
            message: message.into(),
            remediation: None,
        }
    }

    #[must_use]
    pub(crate) fn warning(
        code: &str,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            severity: AuthSecurityDiagnosticSeverity::Warning,
            code: code.to_string(),
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }

    #[must_use]
    pub(crate) fn error(
        code: &str,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            severity: AuthSecurityDiagnosticSeverity::Error,
            code: code.to_string(),
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }
}

/// Result of an auth vault security reconciliation attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthSecurityReconcileReport {
    /// Structured diagnostics to surface through CLI/TUI/status channels.
    pub diagnostics: Vec<AuthSecurityDiagnostic>,
}

/// Read-only auth vault security status.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSecurityStatus {
    /// Vault path inspected.
    pub vault_path: PathBuf,
    /// Auth profile inspected inside the vault.
    pub profile: String,
    /// Configured device-seal policy.
    pub policy: AuthDeviceSealPolicy,
    /// Whether the vault file exists.
    pub vault_exists: bool,
    /// Vault format version when readable.
    #[serde(default)]
    pub vault_version: Option<u8>,
    /// Whether profile-key mode is enabled.
    pub profile_keys_enabled: bool,
    /// Whether the named profile exists in plaintext or profile-entry form.
    pub profile_exists: bool,
    /// Whether the profile policy requires device seal.
    pub profile_device_sealed: bool,
    /// Device-seal factor backend recorded in profile metadata.
    #[serde(default)]
    pub device_seal_backend: Option<String>,
    /// Device-seal policy recorded in profile metadata.
    #[serde(default)]
    pub device_seal_mode: Option<String>,
    /// Strictness recorded in profile metadata.
    #[serde(default)]
    pub device_seal_strict: Option<bool>,
    /// macOS Keychain service recorded in profile metadata when applicable.
    #[serde(default)]
    pub device_seal_keychain_service: Option<String>,
    /// Whether the current config policy is satisfied by this vault state.
    pub policy_satisfied: bool,
    /// Structured diagnostics for this status.
    pub diagnostics: Vec<AuthSecurityDiagnostic>,
}

/// Inspect auth vault security state without mutating the vault.
#[must_use]
pub fn inspect_auth_vault_security(
    vault_path: &Path,
    profile: &str,
    policy: AuthDeviceSealPolicy,
) -> AuthSecurityStatus {
    let status = auth_vault_with_options(vault_path, AuthDeviceSealOptions::from_policy(policy))
        .inspect(profile);
    let diagnostics = status
        .issues
        .iter()
        .map(|issue| security_issue_diagnostic(vault_path, profile, policy, issue))
        .collect();
    AuthSecurityStatus {
        vault_path: status.vault_path,
        profile: status.profile,
        policy: status.policy,
        vault_exists: status.vault_exists,
        vault_version: status.vault_version,
        profile_keys_enabled: status.profile_keys_enabled,
        profile_exists: status.profile_exists,
        profile_device_sealed: status.profile_device_sealed,
        device_seal_backend: status.device_seal_backend,
        device_seal_mode: status.device_seal_mode,
        device_seal_strict: status.device_seal_strict,
        device_seal_keychain_service: status.device_seal_keychain_service,
        policy_satisfied: status.policy_satisfied,
        diagnostics,
    }
}

fn security_issue_diagnostic(
    vault_path: &Path,
    profile: &str,
    policy: AuthDeviceSealPolicy,
    issue: &SecurityIssue,
) -> AuthSecurityDiagnostic {
    match issue {
        SecurityIssue::VaultMissing => AuthSecurityDiagnostic::warning(
            "auth_vault_missing",
            format!(
                "Auth vault does not exist: {}",
                display_from_current_dir(vault_path)
            ),
            "Run `bcode login` for the selected provider.",
        ),
        SecurityIssue::UnlockFailed(_) => AuthSecurityDiagnostic::warning(
            "auth_vault_unlock_failed",
            format!(
                "Auth vault metadata could not be unlocked: {}",
                display_from_current_dir(vault_path)
            ),
            "Ensure the SSH identity used for this vault is available, then retry.",
        ),
        SecurityIssue::ProfileUnlockFailed(error) => AuthSecurityDiagnostic::warning(
            "auth_vault_profile_unlock_failed",
            format!("Auth vault profile {profile} could not be unlocked: {error}"),
            "Restore this device's seal secret, re-login to reset the auth profile, or remove/rebind the profile seal from a device that can unlock it.",
        ),
        SecurityIssue::ProfileMissing => AuthSecurityDiagnostic::warning(
            "auth_vault_profile_missing",
            format!("Auth vault profile {profile} does not exist."),
            "Run `bcode login` for this auth profile.",
        ),
        SecurityIssue::DeviceSealMissing => AuthSecurityDiagnostic {
            severity: if policy == AuthDeviceSealPolicy::Required {
                AuthSecurityDiagnosticSeverity::Error
            } else {
                AuthSecurityDiagnosticSeverity::Warning
            },
            code: "auth_vault_device_seal_missing".to_string(),
            message: format!("Auth vault profile {profile} is not device-sealed."),
            remediation: Some(
                "Add settings.recipient_key for the auth profile if migration is needed, then run `bcode auth status` or `bcode login`.".to_string(),
            ),
        },
        SecurityIssue::StrongerThanPolicy => AuthSecurityDiagnostic::info(
            "auth_vault_stronger_than_config",
            format!(
                "Auth vault profile {profile} is device-sealed even though config sets device_seal=off; stronger vault policy is unchanged."
            ),
        ),
    }
}

pub(crate) fn read_auth_vault_profile(
    vault_path: &Path,
    profile: &str,
) -> Result<Option<BTreeMap<String, String>>, String> {
    let _guard = crate::operations::refresh::native::read_guard(vault_path)
        .map_err(|error| error.to_string())?;
    read_auth_vault_profile_uncoordinated(vault_path, profile)
}

pub(crate) fn read_auth_vault_profile_uncoordinated(
    vault_path: &Path,
    profile: &str,
) -> Result<Option<BTreeMap<String, String>>, String> {
    let (mut vault, data_key) = auth_vault(vault_path).load_metadata().map_err(|error| {
        format!(
            "failed to unlock auth vault metadata at {}: {error}",
            display_from_current_dir(vault_path)
        )
    })?;
    if vault.profiles.get(profile).is_none() && vault.profiles.profile_entries.contains_key(profile)
    {
        vault
            .unlock_profile_with_passphrase(profile, &data_key, None)
            .map_err(|error| format!("failed to unlock auth vault profile {profile}: {error}"))?;
    }
    let values = vault.profiles.get(profile).cloned();
    zeroize_vault_values(&mut vault);
    Ok(values)
}

/// Parse an auth profile's `settings.device_seal` value.
#[must_use]
pub fn device_seal_policy_for_auth_profile(
    auth_profile: &bcode_config::AuthProfileConfig,
) -> AuthDeviceSealPolicy {
    auth_profile
        .settings
        .get("device_seal")
        .map_or(AuthDeviceSealPolicy::Preferred, |value| {
            match value.trim().to_ascii_lowercase().as_str() {
                "off" | "false" | "disabled" | "never" => AuthDeviceSealPolicy::Off,
                "required" | "require" | "true" | "on" => AuthDeviceSealPolicy::Required,
                _ => AuthDeviceSealPolicy::Preferred,
            }
        })
}

/// Parse an auth profile's device-seal settings (`device_seal`, `device_seal_backend`,
/// `device_seal_mode`, `device_seal_strict`).
#[must_use]
pub fn device_seal_options_for_auth_profile(
    auth_profile: &bcode_config::AuthProfileConfig,
) -> AuthDeviceSealOptions {
    let setting = |key: &str| auth_profile.settings.get(key).map(String::as_str);
    AuthDeviceSealOptions::from_settings(
        device_seal_policy_for_auth_profile(auth_profile),
        None,
        setting("device_seal_backend"),
        setting("device_seal_mode"),
        setting("device_seal_strict"),
    )
}

/// Safely reconcile an sshenv auth profile and return structured diagnostics.
#[must_use]
pub fn reconcile_auth_vault_security_report(
    vault_path: &Path,
    profile: &str,
    policy: AuthDeviceSealPolicy,
    explicit_recipient_key: Option<&str>,
) -> AuthSecurityReconcileReport {
    reconcile_auth_vault_security_report_with_options(
        vault_path,
        profile,
        AuthDeviceSealOptions::from_policy(policy),
        explicit_recipient_key,
    )
}

/// Safely reconcile an sshenv auth profile with explicit device-seal options and
/// return structured diagnostics.
#[must_use]
pub fn reconcile_auth_vault_security_report_with_options(
    vault_path: &Path,
    profile: &str,
    options: AuthDeviceSealOptions,
    explicit_recipient_key: Option<&str>,
) -> AuthSecurityReconcileReport {
    let diagnostics = match reconcile_auth_vault_security_with_options(
        vault_path,
        profile,
        options,
        explicit_recipient_key,
    ) {
        Ok(actions) => actions
            .into_iter()
            .map(|action| AuthSecurityDiagnostic::info("auth_vault_security_refreshed", action))
            .collect(),
        Err(error) if options.policy == AuthDeviceSealPolicy::Required => {
            vec![AuthSecurityDiagnostic::error(
                "auth_vault_security_required_unsatisfied",
                format!(
                    "Auth vault security requirement is not satisfied for profile {profile}: {error}"
                ),
                "Run `bcode login` to recreate credentials with the Bcode-managed per-vault key.",
            )]
        }
        Err(error) => vec![AuthSecurityDiagnostic::warning(
            "auth_vault_security_refresh_skipped",
            format!(
                "Auth vault security refresh skipped for profile {profile}; device seal is preferred but not active: {error}"
            ),
            "Run `bcode login` to recreate credentials with the Bcode-managed per-vault key.",
        )],
    };
    AuthSecurityReconcileReport { diagnostics }
}

/// Safely reconcile an sshenv auth profile with the requested device-seal policy.
///
/// This only performs non-destructive upgrades. It may migrate a vault to v2,
/// enable profile-key mode, and add a per-profile device seal. It will not
/// remove an existing device seal when policy is [`AuthDeviceSealPolicy::Off`].
///
/// # Errors
///
/// * The requested policy cannot be applied; callers using
///   [`AuthDeviceSealPolicy::Preferred`] may convert the error into a warning
pub fn reconcile_auth_vault_security(
    vault_path: &Path,
    profile: &str,
    policy: AuthDeviceSealPolicy,
    explicit_recipient_key: Option<&str>,
) -> Result<Vec<String>, String> {
    reconcile_auth_vault_security_with_options(
        vault_path,
        profile,
        AuthDeviceSealOptions::from_policy(policy),
        explicit_recipient_key,
    )
}

/// Safely reconcile an sshenv auth profile with explicit device-seal options.
///
/// This only performs non-destructive upgrades. It may migrate a vault to v2,
/// enable profile-key mode, and add a per-profile device seal. It will not
/// remove an existing device seal when policy is [`AuthDeviceSealPolicy::Off`].
///
/// # Errors
///
/// * The vault metadata cannot be unlocked
/// * The requested policy/options cannot be applied or the result cannot be saved
pub fn reconcile_auth_vault_security_with_options(
    vault_path: &Path,
    profile: &str,
    options: AuthDeviceSealOptions,
    explicit_recipient_key: Option<&str>,
) -> Result<Vec<String>, String> {
    match auth_vault_with_options(vault_path, options).reconcile(profile, explicit_recipient_key) {
        Ok(SecurityReport {
            actions,
            skipped: None,
        }) => Ok(action_messages(profile, &actions)),
        Ok(SecurityReport {
            skipped: Some(reason),
            ..
        })
        | Err(AppVaultError::DeviceSealRequired { reason, .. }) => Err(reason),
        Err(error) => Err(error.to_string()),
    }
}

/// Prepare security changes on the caller's loaded vault without publishing credentials.
/// The caller must discard this vault on failure and retain ownership through publication.
pub(crate) fn prepare_auth_vault_security(
    vault: &mut sshenv_vault::Vault,
    data_key: &[u8; 32],
    vault_path: &Path,
    profile: &str,
    options: AuthDeviceSealOptions,
    explicit_recipient_key: Option<&str>,
) -> Result<(Vec<String>, bool), String> {
    let (actions, changed) = auth_vault_with_options(vault_path, options).prepare_security(
        vault,
        data_key,
        profile,
        explicit_recipient_key,
    )?;
    Ok((action_messages(profile, &actions), changed))
}

fn action_messages(profile: &str, actions: &[SecurityAction]) -> Vec<String> {
    actions
        .iter()
        .map(|action| match action {
            SecurityAction::MigratedToV2 => "migrated auth vault to sshenv v2".to_string(),
            SecurityAction::EnabledProfileKeys => {
                "enabled per-profile auth vault encryption".to_string()
            }
            SecurityAction::BoundDeviceSeal => {
                format!("bound auth profile {profile} to this device")
            }
            SecurityAction::ReboundDeviceSeal => {
                format!("rebound auth profile {profile} to configured device seal")
            }
            SecurityAction::KeptStrongerSeal => format!(
                "auth profile {profile} is device-sealed even though config sets device_seal=off; leaving stronger vault policy unchanged"
            ),
        })
        .collect()
}

/// Prepare current-format retained policy without native factor effects or format conversion.
pub(crate) fn prepare_retained_device_policy(
    vault: &mut sshenv_vault::Vault,
    resolved: &crate::ResolvedAuthProfile,
    profile: &str,
    source: Option<&dyn crate::operations::AuthDeviceFactorSource>,
    intent: Option<&crate::operations::AuthProvisioningIntent>,
) -> Result<Vec<AuthSecurityDiagnostic>, crate::lifecycle::AuthVaultLifecycleError> {
    let options = device_seal_options_for_auth_profile(&resolved.profile);
    if validate_retained_device_policy(vault, profile, options).is_ok() {
        return Ok(Vec::new());
    }
    let mut prepared = vault.clone();
    let result = (|| {
        if prepared.header.version != VERSION_V2 {
            return Err(crate::lifecycle::AuthVaultLifecycleError::WriteFailed(
                "explicit vault migration required".into(),
            ));
        }
        let source = source.ok_or_else(|| {
            crate::lifecycle::AuthVaultLifecycleError::WriteFailed(
                "device provisioning unavailable".into(),
            )
        })?;
        prepared.enable_profile_keys().map_err(|_| {
            crate::lifecycle::AuthVaultLifecycleError::WriteFailed(
                "profile encryption unavailable".into(),
            )
        })?;
        let intent = intent.ok_or_else(|| {
            crate::lifecycle::AuthVaultLifecycleError::WriteFailed(
                "recoverable provisioning unavailable".into(),
            )
        })?;
        let factor = source.provision_attempt(intent)?;
        // Verify the selected custody source can recover the exact key before binding it.
        // This is not a durability guarantee; the source still owns durable factor retention.
        let recovered = source
            .retrieve(
                &factor.id,
                factor.recipient_fingerprint.as_deref(),
                &factor.parameters,
            )
            .map_err(|_| {
                crate::lifecycle::AuthVaultLifecycleError::WriteFailed(
                    "provisioned device factor unavailable".into(),
                )
            })?;
        if *recovered != *factor.key {
            return Err(crate::lifecycle::AuthVaultLifecycleError::WriteFailed(
                "provisioned device factor mismatch".into(),
            ));
        }
        prepared
            .require_profile_device_seal_with(profile, || {
                Ok((
                    sshenv_vault::models::UnlockFactorV2 {
                        id: factor.id,
                        kind: sshenv_vault::models::UnlockFactorKindV2::DeviceSeal,
                        recipient_fingerprint: factor.recipient_fingerprint,
                        params: factor.parameters,
                    },
                    factor.key,
                ))
            })
            .map_err(|_| {
                crate::lifecycle::AuthVaultLifecycleError::WriteFailed(
                    "device binding failed".into(),
                )
            })?;
        validate_retained_device_policy(&prepared, profile, options)
    })();
    if result.is_ok() {
        *vault = prepared;
        return Ok(Vec::new());
    }
    if options.policy == AuthDeviceSealPolicy::Required {
        result?;
    }
    Ok(vec![AuthSecurityDiagnostic::warning(
        "auth_vault_security_refresh_skipped",
        "Preferred device provisioning failed; existing policy preserved",
        "Restore selected device custody to enable the preferred seal",
    )])
}

/// Verify that retained custody can preserve an already-satisfied device policy.
/// No migration, factor creation, or native discovery occurs here.
pub(crate) fn validate_retained_device_policy(
    vault: &sshenv_vault::Vault,
    profile: &str,
    options: AuthDeviceSealOptions,
) -> Result<(), crate::lifecycle::AuthVaultLifecycleError> {
    if options.policy == AuthDeviceSealPolicy::Off
        || (profile_has_device_seal(vault, profile)
            && profile_device_seal_matches(vault, profile, options.seal))
    {
        return Ok(());
    }
    Err(crate::lifecycle::AuthVaultLifecycleError::WriteFailed(
        "retained device policy requires explicit factor provisioning before update".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn windows_dpapi_device_seal_round_trips_for_current_user() {
        let options = sshenv_vault::device::DeviceSealOptions {
            selection: sshenv_vault::device::DeviceSealSelection::Backend(
                sshenv_vault::device::DeviceSealBackendSelection::WindowsDpapiCurrentUser,
            ),
            strict: true,
        };
        let (metadata, created) =
            sshenv_vault::device::create_factor_with_options(options).expect("create DPAPI seal");
        let loaded = sshenv_vault::device::derive_factor_from_metadata(&metadata)
            .expect("load DPAPI seal for current user");
        assert_eq!(&*created, &*loaded);
        assert_eq!(
            metadata.params.get("backend").map(String::as_str),
            Some("windows-dpapi")
        );
    }

    #[test]
    fn explicit_windows_dpapi_backend_is_parsed_without_secret_data() {
        let profile = bcode_config::AuthProfileConfig {
            settings: BTreeMap::from([(
                "device_seal_backend".to_string(),
                "windows-dpapi-current-user".to_string(),
            )]),
            ..bcode_config::AuthProfileConfig::default()
        };
        assert_eq!(
            device_seal_options_for_auth_profile(&profile)
                .seal
                .selection,
            sshenv_vault::device::DeviceSealSelection::Backend(
                sshenv_vault::device::DeviceSealBackendSelection::WindowsDpapiCurrentUser
            )
        );
    }

    #[test]
    fn managed_identity_keeps_bcode_key_file_names() {
        let vault = Path::new("/state/bcode/auth.vault");
        assert_eq!(
            vault_private_key_path(vault),
            Path::new("/state/bcode/auth.vault.keys/bcode_sshenv_ed25519")
        );
        assert_eq!(
            vault_public_key_path(vault),
            Path::new("/state/bcode/auth.vault.keys/bcode_sshenv_ed25519.pub")
        );
    }
}
