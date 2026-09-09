//! Explicit, single-file persistence for caller-owned auth state.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A coordinated subscription and routing snapshot. Secrets remain in the vault.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthState {
    /// Non-secret profile registrations and preferences.
    pub subscriptions: bcode_config::RuntimeAuthSubscriptions,
    /// Routing cursors and observations.
    pub routing: crate::auth_pool_state::AuthPoolState,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    state: AuthState,
}

/// Failure to access or commit explicitly owned auth state.
#[derive(Debug, thiserror::Error)]
pub enum AuthStoreError {
    /// Existing data is unsupported or requires explicit maintenance.
    #[error("auth state is unsupported or invalid; existing state preserved")]
    MaintenanceRequired,
    /// Another owner holds the directory, or ownership cannot be verified.
    #[error("auth state ownership is unavailable")]
    OwnershipUnavailable,
    /// An I/O operation failed. Publication may have occurred before a sync failure.
    #[error("auth state I/O failed")]
    Io(#[source] std::io::Error),
    /// Preference validation failed without committing state.
    #[error("auth preference is invalid")]
    InvalidPreference,
}

/// Exclusive handle to an explicit auth-state directory.
///
/// This is not a Bcode state-root resolver or a legacy-state migration. The caller
/// must supply a dedicated directory and retain this handle while using its state.
/// A missing state file is an error; only `create` initializes storage. Dropping
/// the handle releases ownership. Old subscription files are never imported.
#[derive(Debug)]
pub struct AuthStore {
    directory: PathBuf,
    _lock: File,
}

impl AuthStore {
    /// Initialize a new directory and an empty coordinated snapshot.
    ///
    /// # Errors
    /// Returns an error for relative/existing paths, ownership, or I/O failure.
    /// An interrupted initialization is preserved for explicit maintenance.
    pub fn create(directory: &Path) -> Result<Self, AuthStoreError> {
        if !directory.is_absolute() {
            return Err(AuthStoreError::OwnershipUnavailable);
        }
        fs::create_dir(directory).map_err(AuthStoreError::Io)?;
        let store = Self::acquire(directory)?;
        store.commit(&AuthState::default())?;
        Ok(store)
    }

    /// Open an existing current-format store without repair or initialization.
    ///
    /// # Errors
    /// Returns an error for unverifiable ownership, unsupported state, or I/O failure.
    pub fn open(directory: &Path) -> Result<Self, AuthStoreError> {
        let store = Self::acquire(directory)?;
        store.snapshot()?;
        Ok(store)
    }

    fn acquire(directory: &Path) -> Result<Self, AuthStoreError> {
        if !directory.is_absolute()
            || fs::symlink_metadata(directory)
                .map_err(AuthStoreError::Io)?
                .file_type()
                .is_symlink()
        {
            return Err(AuthStoreError::OwnershipUnavailable);
        }
        let directory = fs::canonicalize(directory).map_err(AuthStoreError::Io)?;
        for entry in fs::read_dir(&directory).map_err(AuthStoreError::Io)? {
            let entry = entry.map_err(AuthStoreError::Io)?;
            if !matches!(
                entry.file_name().to_str(),
                Some("owner.lock" | "state.json")
            ) || !entry.file_type().map_err(AuthStoreError::Io)?.is_file()
            {
                return Err(AuthStoreError::MaintenanceRequired);
            }
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("owner.lock"))
            .map_err(AuthStoreError::Io)?;
        lock.try_lock()
            .map_err(|_| AuthStoreError::OwnershipUnavailable)?;
        Ok(Self {
            directory,
            _lock: lock,
        })
    }

    /// Read a bounded, validated snapshot without changing durable state.
    ///
    /// # Errors
    /// Returns an error for missing, oversized, corrupt, or unsupported state or I/O failure.
    pub fn snapshot(&self) -> Result<AuthState, AuthStoreError> {
        let mut bytes = Vec::new();
        File::open(self.directory.join("state.json"))
            .map_err(AuthStoreError::Io)?
            .take(4_194_305)
            .read_to_end(&mut bytes)
            .map_err(AuthStoreError::Io)?;
        if bytes.len() > 4_194_304 {
            return Err(AuthStoreError::MaintenanceRequired);
        }
        let original: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| AuthStoreError::MaintenanceRequired)?;
        let envelope: Envelope = serde_json::from_value(original.clone())
            .map_err(|_| AuthStoreError::MaintenanceRequired)?;
        if envelope.version != 1
            || serde_json::to_value(&envelope).map_err(|_| AuthStoreError::MaintenanceRequired)?
                != original
        {
            return Err(AuthStoreError::MaintenanceRequired);
        }
        Ok(envelope.state)
    }

    /// Commit a preference and its cursor reset in a single publication.
    ///
    /// # Errors
    /// Returns validation, state, or I/O errors. A directory-sync error after publication
    /// has an uncertain durable outcome; callers must read state before retrying.
    pub fn set_preference(
        &mut self,
        config: &bcode_config::BcodeConfig,
        pool: &str,
        profile: Option<&str>,
    ) -> Result<(), AuthStoreError> {
        let mut state = self.snapshot()?;
        crate::update_auth_pool_preference(
            config,
            &mut state.subscriptions,
            &mut state.routing,
            pool,
            profile,
        )
        .map_err(|_| AuthStoreError::InvalidPreference)?;
        self.commit(&state)
    }

    fn commit(&self, state: &AuthState) -> Result<(), AuthStoreError> {
        let bytes = serde_json::to_vec(&Envelope {
            version: 1,
            state: state.clone(),
        })
        .map_err(|_| AuthStoreError::MaintenanceRequired)?;
        if bytes.len() > 4_194_304 {
            return Err(AuthStoreError::MaintenanceRequired);
        }
        let mut temporary =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(AuthStoreError::Io)?;
        temporary.write_all(&bytes).map_err(AuthStoreError::Io)?;
        temporary.as_file().sync_all().map_err(AuthStoreError::Io)?;
        temporary
            .persist(self.directory.join("state.json"))
            .map_err(|error| AuthStoreError::Io(error.error))?;
        #[cfg(unix)]
        File::open(&self.directory)
            .and_then(|directory| directory.sync_all())
            .map_err(AuthStoreError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_survives_reopen_and_ownership_is_released() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("auth");
        let mut store = AuthStore::create(&path).unwrap();
        assert!(AuthStore::open(&path).is_err());
        let mut config = bcode_config::BcodeConfig::default();
        config.auth.pools.insert(
            "pool".into(),
            bcode_config::AuthPoolConfig {
                profiles: vec!["a".into()],
                ..Default::default()
            },
        );
        assert!(
            store
                .set_preference(&config, "pool", Some("unknown"))
                .is_err()
        );
        assert_eq!(store.snapshot().unwrap(), AuthState::default());
        store.set_preference(&config, "pool", Some("a")).unwrap();
        drop(store);
        let reopened = AuthStore::open(&path).unwrap();
        assert_eq!(
            reopened.snapshot().unwrap().subscriptions.pools["pool"]
                .preferred_profile
                .as_deref(),
            Some("a")
        );
    }
}
