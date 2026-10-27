//! Durable, configuration-scoped account metadata and routing preferences.

use std::collections::BTreeMap;
use std::path::Path;

use bcode_provider_auth_models::{AccountId, AccountPauseState, AccountRoute};
use serde::{Deserialize, Serialize};

/// User-owned metadata for a single account; no credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountMetadata {
    /// Account identity.
    pub account: AccountId,
    /// Optional friendly label.
    pub label: Option<String>,
    /// Persistent user pause, separate from provider cooldowns.
    pub pause: AccountPauseState,
}

/// Canonical user routing state for one effective configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountSettings {
    /// Deduplicated account metadata.
    pub accounts: Vec<AccountMetadata>,
    /// Last verified successful model/auth attempt per session, bounded by known sessions.
    #[serde(default)]
    pub last_used: BTreeMap<String, AccountUsage>,
    /// Session ID to explicit route. Defaults are copied on first session initialization.
    pub sessions: BTreeMap<String, AccountRoute>,
    /// Model preset to new-session default.
    pub defaults: BTreeMap<String, AccountRoute>,
    /// Preset default history; session creation time selects a stable inherited default.
    #[serde(default)]
    pub default_history: BTreeMap<String, Vec<DefaultRoute>>,
}

/// Secret-safe successful provider-attempt evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountUsage {
    /// Actual provider-owned candidate exercised.
    pub account: AccountId,
    /// Provider observation completion timestamp.
    pub observed_at_ms: u64,
}

/// One default change, timestamped independently from session activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultRoute {
    /// Unix milliseconds when this default became effective.
    pub changed_at_ms: u64,
    /// Explicit route for sessions created after this change.
    pub route: AccountRoute,
}

/// Configuration identity excludes mutable account preferences and secrets in external custody.
///
/// # Errors
/// Fails if the effective configuration cannot be serialized.
pub fn configuration_key(
    launch: &super::ConfigLaunchContext,
    context: Option<&str>,
) -> Result<String, super::ConfigError> {
    use sha2::{Digest, Sha256};
    // Identity is source provenance, never serialized settings, model selection, or credentials.
    let sources = launch.recovery_sources.as_ref();
    let bytes = serde_json::to_vec(&(
        &launch.config_files,
        sources.map(|sources| {
            (
                &sources.discovered_paths,
                &sources.exclusive_config_path,
                &sources.base_config_path,
                &sources.env_config_path,
                &sources.cli_config_path,
            )
        }),
        context,
    ))
    .map_err(|_| invalid())?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn invalid() -> super::ConfigError {
    super::ConfigError::Composition {
        message: "account routing state is unavailable or unsupported; existing state preserved"
            .into(),
    }
}

fn read(path: &Path) -> Result<AccountSettings, super::ConfigError> {
    match std::fs::File::open(path) {
        Ok(file) => {
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(1_048_577)
                .read_to_end(&mut bytes)
                .map_err(|_| invalid())?;
            if bytes.len() > 1_048_576 {
                return Err(invalid());
            }
            serde_json::from_slice(&bytes).map_err(|_| invalid())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(AccountSettings::default())
        }
        Err(_) => Err(invalid()),
    }
}

/// Read canonical account settings without creating or repairing state.
///
/// # Errors
/// Fails closed on corrupt, oversized, or unreadable metadata.
pub fn load(root: &Path, key: &str) -> Result<AccountSettings, super::ConfigError> {
    read(&path(root, key)?)
}

fn path(root: &Path, key: &str) -> Result<std::path::PathBuf, super::ConfigError> {
    if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    Ok(root
        .join("auth")
        .join("account-routing")
        .join(format!("{key}.json")))
}

/// Atomically update account-domain settings under the existing durable auth lock discipline.
///
/// # Errors
/// Fails without publication on invalid state, rejected changes, locking, or persistence failure.
pub fn update<T>(
    root: &Path,
    key: &str,
    change: impl FnOnce(&mut AccountSettings) -> Result<T, super::ConfigError>,
) -> Result<T, super::ConfigError> {
    use std::io::Write;
    let path = path(root, key)?;
    let _lock = super::lock_auth_subscriptions(&path)?;
    let mut state = read(&path)?;
    let result = change(&mut state)?;
    let bytes = serde_json::to_vec(&state).map_err(|_| invalid())?;
    if bytes.len() > 1_048_576 {
        return Err(invalid());
    }
    let parent = path.parent().ok_or_else(invalid)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|_| invalid())?;
    temporary.write_all(&bytes).map_err(|_| invalid())?;
    temporary.as_file().sync_all().map_err(|_| invalid())?;
    temporary.persist(&path).map_err(|_| invalid())?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|_| invalid())?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_source_and_context_not_model_or_contents() {
        let launch = crate::ConfigLaunchContext {
            config_files: vec!["/test/provider.toml".into()],
            ..Default::default()
        };
        let key = configuration_key(&launch, Some("work")).unwrap();
        assert_eq!(key, configuration_key(&launch, Some("work")).unwrap());
        assert_ne!(key, configuration_key(&launch, Some("personal")).unwrap());
        let other = crate::ConfigLaunchContext {
            config_files: vec!["/test/other.toml".into()],
            ..launch
        };
        assert_ne!(key, configuration_key(&other, Some("work")).unwrap());
    }

    #[test]
    fn durable_updates_preserve_previous_state_on_rejection_and_corruption() {
        let root = tempfile::tempdir().unwrap();
        let key = "a".repeat(64);
        let route = AccountRoute::Only {
            account: AccountId {
                provider_id: "test".into(),
                profile: "first".into(),
            },
        };
        update(root.path(), &key, |settings| {
            settings.sessions.insert("one".into(), route.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(load(root.path(), &key).unwrap().sessions["one"], route);
        let result: Result<(), _> = update(root.path(), &key, |settings| {
            settings.sessions.clear();
            Err(invalid())
        });
        assert!(result.is_err());
        assert_eq!(load(root.path(), &key).unwrap().sessions["one"], route);
        let file = path(root.path(), &key).unwrap();
        std::fs::write(&file, b"{invalid").unwrap();
        assert!(update(root.path(), &key, |_| Ok(())).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"{invalid");
        assert!(path(root.path(), "../../escape").is_err());
    }
}
