use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use ssh_key::{Algorithm, LineEnding, PrivateKey, rand_core::OsRng};
use sshenv_vault::models::{ProfileFactorRequirement, VERSION_V2};

const BCODE_VAULT_KEY_DIR_SUFFIX: &str = "keys";
const BCODE_VAULT_PRIVATE_KEY_FILE_NAME: &str = "bcode_sshenv_ed25519";
const BCODE_VAULT_PUBLIC_KEY_FILE_NAME: &str = "bcode_sshenv_ed25519.pub";
const BCODE_VAULT_KEY_COMMENT: &str = "bcode sshenv vault key";

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
                private_key.display(),
                public_key.display()
            ),
            Self::OperationFailed { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AuthIdentityError {}

/// Return the Bcode-managed key directory for an sshenv vault path.
#[must_use]
pub fn vault_identity_dir(vault_path: &Path) -> PathBuf {
    let mut dir = vault_path.as_os_str().to_os_string();
    dir.push(".");
    dir.push(BCODE_VAULT_KEY_DIR_SUFFIX);
    PathBuf::from(dir)
}

/// Return the Bcode-managed private key path for an sshenv vault path.
#[must_use]
pub fn vault_private_key_path(vault_path: &Path) -> PathBuf {
    vault_identity_dir(vault_path).join(BCODE_VAULT_PRIVATE_KEY_FILE_NAME)
}

/// Return the Bcode-managed public key path for an sshenv vault path.
#[must_use]
pub fn vault_public_key_path(vault_path: &Path) -> PathBuf {
    vault_identity_dir(vault_path).join(BCODE_VAULT_PUBLIC_KEY_FILE_NAME)
}

/// Return private-key paths Bcode may use to unlock an auth vault.
#[must_use]
pub fn vault_private_key_paths(vault_path: &Path) -> Vec<PathBuf> {
    vec![vault_private_key_path(vault_path)]
}

/// Read the Bcode-managed auth vault recipient key if the complete keypair exists.
///
/// # Errors
///
/// Returns an error when only part of the managed keypair exists or the public
/// key cannot be read.
pub fn read_vault_recipient_key(vault_path: &Path) -> Result<Option<String>, AuthIdentityError> {
    let private_key = vault_private_key_path(vault_path);
    let public_key = vault_public_key_path(vault_path);
    match (private_key.exists(), public_key.exists()) {
        (true, true) => read_public_key_file(&public_key).map(Some),
        (false, false) => Ok(None),
        _ => Err(AuthIdentityError::IncompleteIdentity {
            private_key,
            public_key,
        }),
    }
}

/// Create or reuse the Bcode-managed auth vault recipient key for a vault.
///
/// # Errors
///
/// Returns an error when key generation, filesystem writes, or reading the
/// resulting public key fails.
pub fn ensure_vault_recipient_key(vault_path: &Path) -> Result<String, AuthIdentityError> {
    if let Some(key) = read_vault_recipient_key(vault_path)? {
        return Ok(key);
    }

    let identity_dir = vault_identity_dir(vault_path);
    fs::create_dir_all(&identity_dir).map_err(|error| AuthIdentityError::OperationFailed {
        message: format!(
            "failed to create Bcode auth vault key directory {}: {error}",
            identity_dir.display()
        ),
    })?;
    restrict_dir_permissions(&identity_dir)?;

    let private_key_path = vault_private_key_path(vault_path);
    let public_key_path = vault_public_key_path(vault_path);
    let mut private_key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).map_err(|error| {
        AuthIdentityError::OperationFailed {
            message: format!("failed to generate Bcode auth vault key: {error}"),
        }
    })?;
    private_key.set_comment(BCODE_VAULT_KEY_COMMENT);

    let private_key_text = private_key.to_openssh(LineEnding::LF).map_err(|error| {
        AuthIdentityError::OperationFailed {
            message: format!("failed to encode Bcode auth vault private key: {error}"),
        }
    })?;
    let public_key_text = private_key.public_key().to_openssh().map_err(|error| {
        AuthIdentityError::OperationFailed {
            message: format!("failed to encode Bcode auth vault public key: {error}"),
        }
    })?;

    write_private_key_file(&private_key_path, private_key_text.as_str())?;
    write_public_key_file(&public_key_path, &public_key_text)?;
    read_public_key_file(&public_key_path)
}

fn read_public_key_file(path: &Path) -> Result<String, AuthIdentityError> {
    let contents =
        fs::read_to_string(path).map_err(|error| AuthIdentityError::OperationFailed {
            message: format!(
                "failed to read Bcode auth vault public key {}: {error}",
                path.display()
            ),
        })?;
    contents
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned)
        .ok_or_else(|| AuthIdentityError::OperationFailed {
            message: format!(
                "Bcode auth vault public key {} does not contain a public key line",
                path.display()
            ),
        })
}

fn write_private_key_file(path: &Path, contents: &str) -> Result<(), AuthIdentityError> {
    fs::write(path, contents).map_err(|error| AuthIdentityError::OperationFailed {
        message: format!(
            "failed to write Bcode auth vault private key {}: {error}",
            path.display()
        ),
    })?;
    restrict_private_key_permissions(path)
}

fn write_public_key_file(path: &Path, contents: &str) -> Result<(), AuthIdentityError> {
    fs::write(path, format!("{contents}\n")).map_err(|error| {
        AuthIdentityError::OperationFailed {
            message: format!(
                "failed to write Bcode auth vault public key {}: {error}",
                path.display()
            ),
        }
    })?;
    restrict_public_key_permissions(path)
}

#[cfg(unix)]
fn restrict_dir_permissions(path: &Path) -> Result<(), AuthIdentityError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        AuthIdentityError::OperationFailed {
            message: format!(
                "failed to restrict Bcode auth vault key directory permissions {}: {error}",
                path.display()
            ),
        }
    })
}

#[cfg(not(unix))]
fn restrict_dir_permissions(_path: &Path) -> Result<(), AuthIdentityError> {
    Ok(())
}

#[cfg(unix)]
fn restrict_private_key_permissions(path: &Path) -> Result<(), AuthIdentityError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
        AuthIdentityError::OperationFailed {
            message: format!(
                "failed to restrict Bcode auth vault private key permissions {}: {error}",
                path.display()
            ),
        }
    })
}

#[cfg(not(unix))]
fn restrict_private_key_permissions(_path: &Path) -> Result<(), AuthIdentityError> {
    Ok(())
}

#[cfg(unix)]
fn restrict_public_key_permissions(path: &Path) -> Result<(), AuthIdentityError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).map_err(|error| {
        AuthIdentityError::OperationFailed {
            message: format!(
                "failed to set Bcode auth vault public key permissions {}: {error}",
                path.display()
            ),
        }
    })
}

#[cfg(not(unix))]
fn restrict_public_key_permissions(_path: &Path) -> Result<(), AuthIdentityError> {
    Ok(())
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
    fn info(code: &str, message: impl Into<String>) -> Self {
        Self {
            severity: AuthSecurityDiagnosticSeverity::Info,
            code: code.to_string(),
            message: message.into(),
            remediation: None,
        }
    }

    #[must_use]
    fn warning(code: &str, message: impl Into<String>, remediation: impl Into<String>) -> Self {
        Self {
            severity: AuthSecurityDiagnosticSeverity::Warning,
            code: code.to_string(),
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }

    #[must_use]
    fn error(code: &str, message: impl Into<String>, remediation: impl Into<String>) -> Self {
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
    let mut status = AuthSecurityStatus {
        vault_path: vault_path.to_path_buf(),
        profile: profile.to_string(),
        policy,
        vault_exists: vault_path.exists(),
        vault_version: None,
        profile_keys_enabled: false,
        profile_exists: false,
        profile_device_sealed: false,
        policy_satisfied: policy != AuthDeviceSealPolicy::Required,
        diagnostics: Vec::new(),
    };
    if !status.vault_exists {
        status.diagnostics.push(AuthSecurityDiagnostic::warning(
            "auth_vault_missing",
            format!("Auth vault does not exist: {}", vault_path.display()),
            "Run `bcode login` for the selected provider.",
        ));
        return status;
    }

    if let Ok(ciphertext) = sshenv_vault::Vault::load_ciphertext(vault_path) {
        status.vault_version = Some(ciphertext.header.version);
    }

    let Ok((mut vault, data_key)) = load_auth_vault_metadata(vault_path) else {
        status.diagnostics.push(AuthSecurityDiagnostic::warning(
            "auth_vault_unlock_failed",
            format!(
                "Auth vault metadata could not be unlocked: {}",
                vault_path.display()
            ),
            "Ensure the SSH identity used for this vault is available, then retry.",
        ));
        return status;
    };

    status.vault_version = Some(vault.header.version);
    status.profile_keys_enabled = vault.profile_keys_enabled();
    status.profile_exists = vault.profiles.profiles.contains_key(profile)
        || vault.profiles.profile_entries.contains_key(profile);
    status.profile_device_sealed = profile_has_device_seal(&vault, profile);
    let profile_unlockable = if status.profile_exists
        && vault.profiles.get(profile).is_none()
        && vault.profiles.profile_entries.contains_key(profile)
    {
        match vault.unlock_profile_with_passphrase(profile, &data_key, None) {
            Ok(()) => true,
            Err(error) => {
                status.diagnostics.push(AuthSecurityDiagnostic::warning(
                    "auth_vault_profile_unlock_failed",
                    format!("Auth vault profile {profile} could not be unlocked: {error}"),
                    "Restore this device's seal secret, re-login to reset the auth profile, or remove/rebind the profile seal from a device that can unlock it.",
                ));
                false
            }
        }
    } else {
        true
    };
    status.policy_satisfied = match policy {
        AuthDeviceSealPolicy::Off | AuthDeviceSealPolicy::Preferred => true,
        AuthDeviceSealPolicy::Required => status.profile_device_sealed && profile_unlockable,
    };

    if !status.profile_exists {
        status.diagnostics.push(AuthSecurityDiagnostic::warning(
            "auth_vault_profile_missing",
            format!("Auth vault profile {profile} does not exist."),
            "Run `bcode login` for this auth profile.",
        ));
    } else if policy != AuthDeviceSealPolicy::Off && !status.profile_device_sealed {
        let severity = if policy == AuthDeviceSealPolicy::Required {
            AuthSecurityDiagnosticSeverity::Error
        } else {
            AuthSecurityDiagnosticSeverity::Warning
        };
        status.diagnostics.push(AuthSecurityDiagnostic {
            severity,
            code: "auth_vault_device_seal_missing".to_string(),
            message: format!("Auth vault profile {profile} is not device-sealed."),
            remediation: Some(
                "Add settings.recipient_key for the auth profile if migration is needed, then run `bcode auth status` or `bcode login`.".to_string(),
            ),
        });
    } else if policy == AuthDeviceSealPolicy::Off && status.profile_device_sealed {
        status.diagnostics.push(AuthSecurityDiagnostic::info(
            "auth_vault_stronger_than_config",
            format!(
                "Auth vault profile {profile} is device-sealed even though config sets device_seal=off; stronger vault policy is unchanged."
            ),
        ));
    }

    status
}

pub(crate) fn read_auth_vault_profile(
    vault_path: &Path,
    profile: &str,
) -> Result<Option<BTreeMap<String, String>>, String> {
    let (mut vault, data_key) = load_auth_vault_metadata(vault_path).map_err(|error| {
        format!(
            "failed to unlock auth vault metadata at {}: {error}",
            vault_path.display()
        )
    })?;
    if vault.profiles.get(profile).is_none() && vault.profiles.profile_entries.contains_key(profile)
    {
        vault
            .unlock_profile_with_passphrase(profile, &data_key, None)
            .map_err(|error| format!("failed to unlock auth vault profile {profile}: {error}"))?;
    }
    Ok(vault.profiles.get(profile).cloned())
}

fn load_auth_vault_metadata(
    vault_path: &Path,
) -> Result<(sshenv_vault::Vault, sshenv_vault::DataKey), String> {
    let ciphertext = sshenv_vault::Vault::load_ciphertext(vault_path)
        .map_err(|error| format!("failed to load auth vault: {error}"))?;
    let fingerprints: HashSet<String> = ciphertext
        .recipients
        .iter()
        .map(|recipient| recipient.fingerprint.clone())
        .collect();
    let private_key_paths = vault_private_key_paths(vault_path);
    let identities = sshenv_vault::identity::load_identities_for_vault_from_paths(
        &private_key_paths,
        &fingerprints,
    )
    .map_err(|error| format!("failed to load Bcode auth vault SSH identity: {error}"))?;
    if identities.is_empty() {
        return Err(
            "no Bcode-managed auth vault private key matches an auth vault recipient; run `bcode login` to recreate this profile"
                .to_string(),
        );
    }
    sshenv_vault::Vault::unlock_metadata_with_passphrase(ciphertext, &identities, None)
        .map_err(|error| format!("failed to unlock auth vault metadata: {error}"))
}

/// Desired device-seal policy for an sshenv-backed auth profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthDeviceSealPolicy {
    /// Do not add a device seal automatically.
    Off,
    /// Add a device seal when the local host can support it, but continue on failure.
    Preferred,
    /// Add a device seal and treat failure as a policy violation.
    Required,
}

/// Complete device-seal configuration for an sshenv-backed auth profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthDeviceSealOptions {
    /// Whether Bcode should reconcile a profile device seal.
    pub policy: AuthDeviceSealPolicy,
    /// Concrete sshenv device-seal selection and strictness.
    pub seal: sshenv_vault::device::DeviceSealOptions,
}

impl AuthDeviceSealOptions {
    /// Default Bcode auth-vault device-seal behavior.
    #[must_use]
    pub const fn preferred_transparent_device_only() -> Self {
        Self {
            policy: AuthDeviceSealPolicy::Preferred,
            seal: sshenv_vault::device::DeviceSealOptions {
                selection: sshenv_vault::device::DeviceSealSelection::Policy(
                    sshenv_vault::device::DeviceSealPolicy::TransparentDeviceOnly,
                ),
                strict: true,
            },
        }
    }

    /// Build options from the legacy policy-only API.
    #[must_use]
    pub const fn from_policy(policy: AuthDeviceSealPolicy) -> Self {
        Self {
            policy,
            ..Self::preferred_transparent_device_only()
        }
    }
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

/// Parse an auth profile's device-seal settings.
#[must_use]
pub fn device_seal_options_for_auth_profile(
    auth_profile: &bcode_config::AuthProfileConfig,
) -> AuthDeviceSealOptions {
    let policy = device_seal_policy_for_auth_profile(auth_profile);
    let selection = auth_profile
        .settings
        .get("device_seal_backend")
        .and_then(|value| parse_device_seal_backend(value))
        .map_or_else(
            || {
                sshenv_vault::device::DeviceSealSelection::Policy(
                    auth_profile
                        .settings
                        .get("device_seal_mode")
                        .and_then(|value| parse_device_seal_policy(value))
                        .unwrap_or(sshenv_vault::device::DeviceSealPolicy::TransparentDeviceOnly),
                )
            },
            sshenv_vault::device::DeviceSealSelection::Backend,
        );
    let strict = auth_profile
        .settings
        .get("device_seal_strict")
        .is_none_or(|value| parse_bool_setting(value, true));
    AuthDeviceSealOptions {
        policy,
        seal: sshenv_vault::device::DeviceSealOptions { selection, strict },
    }
}

fn parse_device_seal_policy(value: &str) -> Option<sshenv_vault::device::DeviceSealPolicy> {
    match normalize_setting(value).as_str() {
        "default" | "legacy" => Some(sshenv_vault::device::DeviceSealPolicy::Default),
        "transparentdeviceonly" | "transparentlocaldevice" | "devicelocaltransparent" => {
            Some(sshenv_vault::device::DeviceSealPolicy::TransparentDeviceOnly)
        }
        _ => None,
    }
}

fn parse_device_seal_backend(
    value: &str,
) -> Option<sshenv_vault::device::DeviceSealBackendSelection> {
    match normalize_setting(value).as_str() {
        "macoskeychain" => Some(sshenv_vault::device::DeviceSealBackendSelection::MacosKeychain),
        "macoskeychaindeviceonly" => {
            Some(sshenv_vault::device::DeviceSealBackendSelection::MacosKeychainDeviceOnly)
        }
        "windowsdpapi" | "windowsdpapicurrentuser" => {
            Some(sshenv_vault::device::DeviceSealBackendSelection::WindowsDpapiCurrentUser)
        }
        "linuxtpm" | "tpm" => Some(sshenv_vault::device::DeviceSealBackendSelection::LinuxTpm),
        "linuxsecretservice" | "secretservice" => {
            Some(sshenv_vault::device::DeviceSealBackendSelection::LinuxSecretService)
        }
        "secureenclave" => Some(sshenv_vault::device::DeviceSealBackendSelection::SecureEnclave),
        "localfile" => Some(sshenv_vault::device::DeviceSealBackendSelection::LocalFile),
        _ => None,
    }
}

fn parse_bool_setting(value: &str, default: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "strict" => true,
        "0" | "false" | "no" | "off" | "relaxed" => false,
        _ => default,
    }
}

fn normalize_setting(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
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
    match reconcile_auth_vault_security_with_options(
        vault_path,
        profile,
        options,
        explicit_recipient_key,
    ) {
        Ok(actions) => AuthSecurityReconcileReport {
            diagnostics: actions
                .into_iter()
                .map(|action| AuthSecurityDiagnostic::info("auth_vault_security_refreshed", action))
                .collect(),
        },
        Err(error) => {
            let severity = if options.policy == AuthDeviceSealPolicy::Required {
                AuthSecurityDiagnosticSeverity::Error
            } else {
                AuthSecurityDiagnosticSeverity::Warning
            };
            let diagnostic = match severity {
                AuthSecurityDiagnosticSeverity::Info => {
                    AuthSecurityDiagnostic::info("auth_vault_security_refresh_skipped", error)
                }
                AuthSecurityDiagnosticSeverity::Warning => AuthSecurityDiagnostic::warning(
                    "auth_vault_security_refresh_skipped",
                    format!(
                        "Auth vault security refresh skipped for profile {profile}; device seal is preferred but not active: {error}"
                    ),
                    "Run `bcode login` to recreate credentials with the Bcode-managed per-vault key.",
                ),
                AuthSecurityDiagnosticSeverity::Error => AuthSecurityDiagnostic::error(
                    "auth_vault_security_required_unsatisfied",
                    format!(
                        "Auth vault security requirement is not satisfied for profile {profile}: {error}"
                    ),
                    "Run `bcode login` to recreate credentials with the Bcode-managed per-vault key.",
                ),
            };
            AuthSecurityReconcileReport {
                diagnostics: vec![diagnostic],
            }
        }
    }
}

/// Safely reconcile an sshenv auth profile with the requested device-seal policy.
///
/// This only performs non-destructive upgrades. It may migrate a vault to v2,
/// enable profile-key mode, and add a per-profile device seal. It will not
/// remove an existing device seal when policy is [`AuthDeviceSealPolicy::Off`].
///
/// # Errors
///
/// Returns an error when the requested policy cannot be applied. Callers using
/// [`AuthDeviceSealPolicy::Preferred`] may convert the error into a warning.
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
/// Returns an error when the requested policy/options cannot be applied.
pub fn reconcile_auth_vault_security_with_options(
    vault_path: &Path,
    profile: &str,
    options: AuthDeviceSealOptions,
    explicit_recipient_key: Option<&str>,
) -> Result<Vec<String>, String> {
    if !vault_path.exists() {
        return Ok(Vec::new());
    }

    let (mut vault, data_key) = load_auth_vault_metadata(vault_path)
        .map_err(|error| format!("failed to unlock auth vault metadata: {error}"))?;

    let mut actions = Vec::new();
    if matches!(options.policy, AuthDeviceSealPolicy::Off) {
        if profile_has_device_seal(&vault, profile) {
            actions.push(format!(
                "auth profile {profile} is device-sealed even though config sets device_seal=off; leaving stronger vault policy unchanged"
            ));
        }
        return Ok(actions);
    }

    if !vault.profiles.profiles.contains_key(profile)
        && !vault.profiles.profile_entries.contains_key(profile)
    {
        return Ok(actions);
    }

    if profile_has_device_seal(&vault, profile) {
        return Ok(actions);
    }

    if vault.profiles.get(profile).is_none() && vault.profiles.profile_entries.contains_key(profile)
    {
        vault
            .unlock_profile_with_passphrase(profile, &data_key, None)
            .map_err(|error| format!("failed to unlock auth profile {profile}: {error}"))?;
    }

    if vault.header.version != VERSION_V2 {
        let recipient_keys = recipient_keys_for_vault(vault_path, &vault, explicit_recipient_key)?;
        vault
            .migrate_to_v2(&recipient_keys)
            .map_err(|error| format!("failed to migrate auth vault to v2: {error}"))?;
        actions.push("migrated auth vault to sshenv v2".to_string());
    }

    if !vault.profile_keys_enabled() {
        vault
            .enable_profile_keys()
            .map_err(|error| format!("failed to enable auth profile keys: {error}"))?;
        actions.push("enabled per-profile auth vault encryption".to_string());
    }

    vault
        .require_profile_device_seal_with_options(profile, options.seal)
        .map_err(|error| {
            format!("failed to bind auth profile {profile} to this device: {error}")
        })?;
    actions.push(format!("bound auth profile {profile} to this device"));
    vault
        .save(vault_path, &data_key)
        .map_err(|error| format!("failed to save reconciled auth vault: {error}"))?;

    Ok(actions)
}

fn profile_has_device_seal(vault: &sshenv_vault::Vault, profile: &str) -> bool {
    vault
        .profiles
        .profile_policy(profile)
        .is_some_and(|policy| {
            policy
                .required_factors
                .contains(&ProfileFactorRequirement::DeviceSeal)
        })
}

fn recipient_keys_for_vault(
    vault_path: &Path,
    vault: &sshenv_vault::Vault,
    explicit_recipient_key: Option<&str>,
) -> Result<Vec<String>, String> {
    let expected: BTreeSet<_> = vault
        .recipients
        .iter()
        .map(|recipient| recipient.fingerprint.clone())
        .collect();
    let candidates = recipient_key_candidates(vault_path, explicit_recipient_key);
    let by_fingerprint: BTreeMap<_, _> = candidates
        .into_iter()
        .filter_map(|line| {
            fingerprint_for_public_key_line(&line).map(|fingerprint| (fingerprint, line))
        })
        .collect();

    let mut keys = Vec::new();
    let mut missing = Vec::new();
    for fingerprint in expected {
        if let Some(line) = by_fingerprint.get(&fingerprint) {
            keys.push(line.clone());
        } else {
            missing.push(fingerprint);
        }
    }
    if missing.is_empty() {
        Ok(keys)
    } else {
        Err(format!(
            "cannot migrate existing auth vault to v2 because recipient public keys were not found for: {}. Run `bcode login` to recreate credentials with the Bcode-managed per-vault key, or pass --recipient-key if this vault intentionally uses a custom key.",
            missing.join(", ")
        ))
    }
}

fn recipient_key_candidates(
    vault_path: &Path,
    explicit_recipient_key: Option<&str>,
) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(key) = explicit_recipient_key.and_then(read_public_key_arg) {
        candidates.push(key);
    } else if let Ok(Some(key)) = read_vault_recipient_key(vault_path) {
        candidates.push(key);
    }
    candidates
}

fn read_public_key_arg(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.starts_with("ssh-") {
        return Some(trimmed.to_string());
    }
    fs::read_to_string(trimmed).ok()
}

fn fingerprint_for_public_key_line(line: &str) -> Option<String> {
    let body = line.split_whitespace().nth(1)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body)
        .ok()?;
    let hash = Sha256::digest(decoded);
    let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(hash);
    Some(format!("SHA256:{encoded}"))
}
