//! Explicit, single-file persistence for caller-owned auth state.

use std::fs::{self, File, OpenOptions};
use std::io::Read as _;
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
    storage: Box<dyn AuthStorage>,
}

#[cfg(feature = "simulation")]
mod simulation;
#[cfg(feature = "simulation")]
pub use simulation::AuthSimulation;

/// An exclusively acquired auth snapshot location, retained until the store is dropped.
///
/// Implementations must retain verified ownership, confine access to their acquired
/// location, preserve unsupported data, and release all ownership on drop. Reads must
/// be bounded and non-mutating; publication must not silently change storage locations.
pub trait AuthStorage: std::fmt::Debug + Send + Sync {
    /// Read at most `limit` bytes of the current snapshot without repairing it.
    ///
    /// # Errors
    /// Returns missing-state, ownership, or storage access errors.
    fn read_snapshot(&self, limit: usize) -> Result<Vec<u8>, AuthStoreError>;

    /// Atomically publish bytes while preserving exclusive ownership.
    ///
    /// # Errors
    /// Returns publication errors; a post-publication error may have an uncertain outcome.
    fn publish_snapshot(&self, bytes: &[u8]) -> Result<(), AuthStoreError>;
}

#[derive(Debug)]
struct NativeAuthStorage {
    directory: PathBuf,
    _lock: File,
}

impl AuthStore {
    /// Initialize a new directory and an empty coordinated snapshot.
    ///
    /// Unix directories are created with owner-only permissions before writing state.
    /// Other platforms use their native inherited directory permissions. The caller
    /// must control the parent and ancestors; this does not prevent path replacement.
    ///
    /// # Errors
    /// Returns an error for relative/existing paths, ownership, or I/O failure.
    /// An interrupted initialization is preserved for explicit maintenance.
    pub fn create(directory: &Path) -> Result<Self, AuthStoreError> {
        if !directory.is_absolute() {
            return Err(AuthStoreError::OwnershipUnavailable);
        }
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(directory).map_err(AuthStoreError::Io)?;
        let store = Self::acquire(directory, true)?;
        store.commit(&AuthState::default())?;
        Ok(store)
    }

    /// Open an existing current-format store without repair or initialization.
    ///
    /// # Errors
    /// Returns an error for unverifiable ownership, unsupported state, or I/O failure.
    pub fn open(directory: &Path) -> Result<Self, AuthStoreError> {
        let store = Self::acquire(directory, false)?;
        store.snapshot()?;
        Ok(store)
    }

    fn acquire(directory: &Path, initialize: bool) -> Result<Self, AuthStoreError> {
        if !directory.is_absolute()
            || fs::symlink_metadata(directory)
                .map_err(AuthStoreError::Io)?
                .file_type()
                .is_symlink()
        {
            return Err(AuthStoreError::OwnershipUnavailable);
        }
        let directory = fs::canonicalize(directory).map_err(AuthStoreError::Io)?;
        let lock_path = directory.join("owner.lock");
        if !initialize
            && !fs::symlink_metadata(&lock_path)
                .map_err(AuthStoreError::Io)?
                .file_type()
                .is_file()
        {
            return Err(AuthStoreError::OwnershipUnavailable);
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(initialize)
            .open(lock_path)
            .map_err(AuthStoreError::Io)?;
        lock.try_lock()
            .map_err(|_| AuthStoreError::OwnershipUnavailable)?;
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
        Ok(Self {
            storage: Box::new(NativeAuthStorage {
                directory,
                _lock: lock,
            }),
        })
    }

    /// Open an already acquired storage owner using the shared current-format validation.
    ///
    /// This does not initialize, repair, or select an alternate location. Ownership is
    /// released if validation fails.
    ///
    /// # Errors
    /// Returns snapshot access, size, or format validation errors.
    pub fn from_storage(storage: Box<dyn AuthStorage>) -> Result<Self, AuthStoreError> {
        let store = Self { storage };
        store.snapshot()?;
        Ok(store)
    }

    /// Read a bounded, validated snapshot without changing durable state.
    ///
    /// # Errors
    /// Returns an error for missing, oversized, corrupt, or unsupported state or I/O failure.
    pub fn snapshot(&self) -> Result<AuthState, AuthStoreError> {
        let bytes = self.storage.read_snapshot(4_194_305)?;
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

    /// Resolve provider authentication using the current owned subscription snapshot.
    ///
    /// Profile materialization is supplied by the caller; this method never falls back
    /// to ambient subscriptions or native credential discovery. Canonical pool selection
    /// remains in the provider-auth resolver. The materializer owns its credential effects.
    ///
    /// # Errors
    /// Returns snapshot access or validation errors before invoking the materializer.
    pub fn resolve_provider_context(
        &self,
        request: crate::ProviderRequestContextResolution<'_>,
        resolve: impl FnMut(&str, &bcode_config::AuthProfileConfig) -> crate::ResolvedProviderAuth,
    ) -> Result<bcode_model::ProviderRequestContext, AuthStoreError> {
        let snapshot = self.snapshot()?;
        Ok(crate::resolve_provider_request_context_with_resolver(
            request,
            &snapshot.subscriptions,
            resolve,
        ))
    }

    /// Mutate subscriptions and routing together while retaining exclusive ownership.
    ///
    /// The callback receives a fresh validated snapshot. Returning an error discards
    /// its changes. Callbacks must not perform external side effects: publication
    /// can fail after the callback completes.
    ///
    /// # Errors
    /// Returns snapshot, callback, serialization, or publication errors. Publication
    /// errors can have an uncertain durable outcome; read state before retrying.
    pub fn update(
        &mut self,
        mutation: impl FnOnce(&mut AuthState) -> Result<(), AuthStoreError>,
    ) -> Result<(), AuthStoreError> {
        let mut state = self.snapshot()?;
        mutation(&mut state)?;
        self.commit(&state)
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
        self.update(|state| {
            crate::update_auth_pool_preference(
                config,
                &mut state.subscriptions,
                &mut state.routing,
                pool,
                profile,
            )
            .map_err(|_| AuthStoreError::InvalidPreference)
        })
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
        self.storage.publish_snapshot(&bytes)
    }
}

impl AuthStorage for NativeAuthStorage {
    fn read_snapshot(&self, limit: usize) -> Result<Vec<u8>, AuthStoreError> {
        let mut bytes = Vec::new();
        File::open(self.directory.join("state.json"))
            .map_err(AuthStoreError::Io)?
            .take(limit as u64)
            .read_to_end(&mut bytes)
            .map_err(AuthStoreError::Io)?;
        Ok(bytes)
    }

    fn publish_snapshot(&self, bytes: &[u8]) -> Result<(), AuthStoreError> {
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use switchy_fs::standard::sync::{create_private_file, rename_file, sync_directory};
            // Acquisition and reads are native. Never let Cargo feature unification
            // select a different namespace for publication.
            let pending = self.directory.join("pending.json");
            let mut file = create_private_file(&pending).map_err(AuthStoreError::Io)?;
            file.write_all(bytes).map_err(AuthStoreError::Io)?;
            file.sync_all().map_err(AuthStoreError::Io)?;
            drop(file);
            rename_file(&pending, self.directory.join("state.json")).map_err(AuthStoreError::Io)?;
            sync_directory(&self.directory).map_err(AuthStoreError::Io)
        }
        #[cfg(not(unix))]
        {
            use std::io::Write as _;
            let mut temporary =
                tempfile::NamedTempFile::new_in(&self.directory).map_err(AuthStoreError::Io)?;
            temporary.write_all(bytes).map_err(AuthStoreError::Io)?;
            temporary.as_file().sync_all().map_err(AuthStoreError::Io)?;
            temporary
                .persist(self.directory.join("state.json"))
                .map_err(|error| AuthStoreError::Io(error.error))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn publication_collision_preserves_state_and_requires_maintenance_on_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("auth");
        let mut store = AuthStore::create(&path).unwrap();
        let before = fs::read(path.join("state.json")).unwrap();
        fs::write(path.join("pending.json"), b"unowned").unwrap();
        assert!(store.update(|_| Ok(())).is_err());
        assert_eq!(fs::read(path.join("pending.json")).unwrap(), b"unowned");
        assert_eq!(fs::read(path.join("state.json")).unwrap(), before);
        drop(store);
        assert!(matches!(
            AuthStore::open(&path),
            Err(AuthStoreError::MaintenanceRequired)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn creation_is_private_and_does_not_modify_existing_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("auth");
        let store = AuthStore::create(&path).unwrap();
        for entry in [&path, &path.join("state.json")] {
            assert_eq!(fs::metadata(entry).unwrap().permissions().mode() & 0o077, 0);
        }
        drop(store);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        let before = fs::read(path.join("state.json")).unwrap();
        assert!(AuthStore::create(&path).is_err());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_eq!(fs::read(path.join("state.json")).unwrap(), before);
    }

    #[test]
    fn missing_lock_is_not_recreated_and_failed_mutation_is_not_committed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("auth");
        let mut store = AuthStore::create(&path).unwrap();
        let before = fs::read(path.join("state.json")).unwrap();
        assert!(
            store
                .update(|state| {
                    state.routing.pools.insert(
                        "pool".into(),
                        crate::auth_pool_state::AuthPoolRoutingState::default(),
                    );
                    Err(AuthStoreError::InvalidPreference)
                })
                .is_err()
        );
        assert_eq!(fs::read(path.join("state.json")).unwrap(), before);
        drop(store);
        fs::remove_file(path.join("owner.lock")).unwrap();
        assert!(AuthStore::open(&path).is_err());
        assert!(!path.join("owner.lock").exists());
        assert_eq!(fs::read(path.join("state.json")).unwrap(), before);
    }

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
