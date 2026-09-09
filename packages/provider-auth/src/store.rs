//! Explicit, single-file persistence for caller-owned auth state.

use std::fs::{self, File, OpenOptions};
use std::io::Read as _;
use std::path::Path;
#[cfg(not(unix))]
use std::path::PathBuf;

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
/// On Unix, snapshot reads and publication remain relative to a retained directory
/// handle even if its pathname is replaced. Acquisition still requires caller-controlled
/// ancestors and excludes nonparticipating writers only by that ownership precondition.
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
    #[cfg(not(unix))]
    directory: PathBuf,
    #[cfg(unix)]
    handle: File,
    _lock: File,
}

#[cfg(unix)]
fn open_owned_entry(directory: &File, name: &std::ffi::CStr, flags: i32) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    // SAFETY: directory is live, name is NUL-terminated, and mode is supplied for creation.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor owned exclusively by this File.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::other("auth entry is not a regular file"));
    }
    Ok(file)
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
        #[cfg(unix)]
        let handle = {
            use std::os::unix::fs::OpenOptionsExt as _;
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&directory)
                .map_err(AuthStoreError::Io)?
        };
        #[cfg(unix)]
        let lock = open_owned_entry(
            &handle,
            c"owner.lock",
            libc::O_RDWR
                | if initialize {
                    libc::O_CREAT | libc::O_EXCL
                } else {
                    0
                },
        )
        .map_err(AuthStoreError::Io)?;
        #[cfg(not(unix))]
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let held = handle.metadata().map_err(AuthStoreError::Io)?;
            let named = fs::symlink_metadata(&directory).map_err(AuthStoreError::Io)?;
            // SAFETY: geteuid has no preconditions and does not mutate process state.
            let owner = unsafe { libc::geteuid() };
            if held.dev() != named.dev()
                || held.ino() != named.ino()
                || held.uid() != owner
                || held.mode() & 0o077 != 0
            {
                return Err(AuthStoreError::OwnershipUnavailable);
            }
        }
        Ok(Self {
            storage: Box::new(NativeAuthStorage {
                #[cfg(not(unix))]
                directory,
                #[cfg(unix)]
                handle,
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
        #[cfg(unix)]
        let file = open_owned_entry(&self.handle, c"state.json", libc::O_RDONLY)
            .map_err(AuthStoreError::Io)?;
        #[cfg(not(unix))]
        let file = File::open(self.directory.join("state.json")).map_err(AuthStoreError::Io)?;
        file.take(limit as u64)
            .read_to_end(&mut bytes)
            .map_err(AuthStoreError::Io)?;
        Ok(bytes)
    }

    fn publish_snapshot(&self, bytes: &[u8]) -> Result<(), AuthStoreError> {
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::fd::AsRawFd as _;
            let mut file = open_owned_entry(
                &self.handle,
                c"pending.json",
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            )
            .map_err(AuthStoreError::Io)?;
            file.write_all(bytes).map_err(AuthStoreError::Io)?;
            file.sync_all().map_err(AuthStoreError::Io)?;
            drop(file);
            // SAFETY: both names are static C strings relative to the retained directory.
            let result = unsafe {
                libc::renameat(
                    self.handle.as_raw_fd(),
                    c"pending.json".as_ptr(),
                    self.handle.as_raw_fd(),
                    c"state.json".as_ptr(),
                )
            };
            if result != 0 {
                return Err(AuthStoreError::Io(std::io::Error::last_os_error()));
            }
            self.handle.sync_all().map_err(AuthStoreError::Io)
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
    fn retained_directory_prevents_path_replacement_redirecting_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("auth");
        let moved = temp.path().join("moved");
        let mut store = AuthStore::create(&path).unwrap();
        fs::rename(&path, &moved).unwrap();
        let replacement = AuthStore::create(&path).unwrap();
        store
            .update(|state| {
                state.routing.pools.insert(
                    "original".into(),
                    crate::auth_pool_state::AuthPoolRoutingState::default(),
                );
                Ok(())
            })
            .unwrap();
        assert!(
            store
                .snapshot()
                .unwrap()
                .routing
                .pools
                .contains_key("original")
        );
        assert_eq!(replacement.snapshot().unwrap(), AuthState::default());
        assert!(AuthStore::open(&moved).is_err());
        drop(store);
        assert!(
            AuthStore::open(&moved)
                .unwrap()
                .snapshot()
                .unwrap()
                .routing
                .pools
                .contains_key("original")
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_symlink_is_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("auth");
        let store = AuthStore::create(&path).unwrap();
        let outside = temp.path().join("outside.json");
        fs::rename(path.join("state.json"), &outside).unwrap();
        std::os::unix::fs::symlink(&outside, path.join("state.json")).unwrap();
        assert!(store.snapshot().is_err());
    }

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
