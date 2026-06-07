use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sshenv_vault::models::{ProfileFactorRequirement, VERSION_V2};

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

    let store = sshenv_vault::SshenvStore::new(sshenv_vault::SshenvStoreConfig::new(
        vault_path.to_path_buf(),
    ));
    let Ok((vault, _data_key)) = store.load_and_unlock() else {
        status.diagnostics.push(AuthSecurityDiagnostic::warning(
            "auth_vault_unlock_failed",
            format!("Auth vault could not be unlocked: {}", vault_path.display()),
            "Ensure the SSH identity used for this vault is available, then retry.",
        ));
        return status;
    };

    status.vault_version = Some(vault.header.version);
    status.profile_keys_enabled = vault.profile_keys_enabled();
    status.profile_exists = vault.profiles.profiles.contains_key(profile)
        || vault.profiles.profile_entries.contains_key(profile);
    status.profile_device_sealed = profile_has_device_seal(&vault, profile);
    status.policy_satisfied = match policy {
        AuthDeviceSealPolicy::Off | AuthDeviceSealPolicy::Preferred => true,
        AuthDeviceSealPolicy::Required => status.profile_device_sealed,
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

/// Safely reconcile an sshenv auth profile and return structured diagnostics.
#[must_use]
pub fn reconcile_auth_vault_security_report(
    vault_path: &Path,
    profile: &str,
    policy: AuthDeviceSealPolicy,
    explicit_recipient_key: Option<&str>,
) -> AuthSecurityReconcileReport {
    match reconcile_auth_vault_security(vault_path, profile, policy, explicit_recipient_key) {
        Ok(actions) => AuthSecurityReconcileReport {
            diagnostics: actions
                .into_iter()
                .map(|action| AuthSecurityDiagnostic::info("auth_vault_security_refreshed", action))
                .collect(),
        },
        Err(error) => {
            let severity = if policy == AuthDeviceSealPolicy::Required {
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
                    "Add settings.recipient_key for the auth profile or run `bcode auth status` for details.",
                ),
                AuthSecurityDiagnosticSeverity::Error => AuthSecurityDiagnostic::error(
                    "auth_vault_security_required_unsatisfied",
                    format!(
                        "Auth vault security requirement is not satisfied for profile {profile}: {error}"
                    ),
                    "Add settings.recipient_key for the auth profile, ensure local secure storage is available, then retry.",
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
    if !vault_path.exists() {
        return Ok(Vec::new());
    }

    let store = sshenv_vault::SshenvStore::new(sshenv_vault::SshenvStoreConfig::new(
        vault_path.to_path_buf(),
    ));
    let (mut vault, data_key) = store
        .load_and_unlock()
        .map_err(|error| format!("failed to unlock auth vault: {error}"))?;

    let mut actions = Vec::new();
    if matches!(policy, AuthDeviceSealPolicy::Off) {
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

    if vault.header.version != VERSION_V2 {
        let recipient_keys = recipient_keys_for_vault(&vault, explicit_recipient_key)?;
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
        .require_profile_device_seal(profile)
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
    vault: &sshenv_vault::Vault,
    explicit_recipient_key: Option<&str>,
) -> Result<Vec<String>, String> {
    let expected: BTreeSet<_> = vault
        .recipients
        .iter()
        .map(|recipient| recipient.fingerprint.clone())
        .collect();
    let candidates = recipient_key_candidates(explicit_recipient_key);
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
            "cannot migrate existing auth vault to v2 because recipient public keys were not found for: {}. Re-run login with --recipient-key PATH_TO_PUBLIC_KEY or add settings.recipient_key to the auth profile.",
            missing.join(", ")
        ))
    }
}

fn recipient_key_candidates(explicit_recipient_key: Option<&str>) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(key) = explicit_recipient_key.and_then(read_public_key_arg) {
        candidates.push(key);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let ssh_dir = PathBuf::from(home).join(".ssh");
        if let Ok(entries) = fs::read_dir(ssh_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|extension| extension == "pub")
                    && let Ok(line) = fs::read_to_string(path)
                {
                    candidates.push(line);
                }
            }
        }
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
