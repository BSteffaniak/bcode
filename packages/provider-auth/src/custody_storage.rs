//! Explicit storage for encrypted credential custody, separate from subscription state.

use std::io::{Read as _, Write as _};
use std::path::Path;

use bcode_confined_fs::{Creation, Dir, OpenOptions, RenameMode};

#[cfg(feature = "simulation")]
pub mod simulation;

const MAGIC: &[u8] = b"BCODE-CUSTODY\0\x01";
const MAX_BYTES: usize = 4 * 1024 * 1024;

/// Secret-safe custody storage failure.
#[derive(Debug, thiserror::Error)]
pub enum CustodyStorageError {
    /// Ownership cannot be verified.
    #[error("credential custody ownership unavailable")]
    Ownership,
    /// Preserve unsupported, damaged, or interrupted state for maintenance.
    #[error("credential custody requires maintenance")]
    MaintenanceRequired,
    /// The caller's ciphertext no longer matches stored state.
    #[error("credential custody changed; reload before retrying")]
    Conflict,
    /// Publication may have occurred if its final durability barrier failed.
    #[error("credential custody storage failed")]
    Io(#[source] std::io::Error),
}

/// Retained ownership of encrypted credential state and its provisioning fence.
///
/// Implementations must confine access to an exclusively acquired location, bound reads,
/// preserve unsupported state, and release all ownership on drop. Publication must compare
/// expected ciphertext and atomically replace it under that ownership. Native implementations
/// must durably publish both ciphertext and provisioning intents; simulated implementations
/// must explicitly document their durability limits. Errors must never trigger native fallback.
/// This contract selects storage effects only, not vault policy or credential mapping.
pub trait AuthCustodyStorage: Send {
    /// Read bounded, validated current-format vault ciphertext without mutation or repair.
    ///
    /// # Errors
    /// Returns errors for unavailable, damaged, or unsupported state.
    fn read(&self) -> Result<Vec<u8>, CustodyStorageError>;

    /// Compare and publish validated ciphertext, preserving unrelated state.
    ///
    /// # Errors
    /// Rejects stale expected bytes or invalid state. Failure after publication is uncertain
    /// and must not be interpreted as rollback.
    fn compare_and_publish(
        &mut self,
        expected: &[u8],
        ciphertext: &[u8],
    ) -> Result<(), CustodyStorageError>;

    /// Verify no unresolved provisioning fence exists before admitting mutation.
    ///
    /// # Errors
    /// Fails closed if absence cannot be verified.
    fn ensure_no_provisioning(&self) -> Result<(), CustodyStorageError>;

    /// Exclusively publish an intent before invoking external provisioning effects.
    ///
    /// # Errors
    /// Rejects existing, unsupported, or unpublishable intents without replacing a fence.
    fn begin_provisioning(
        &self,
        intent: &crate::operations::AuthProvisioningIntent,
    ) -> Result<(), CustodyStorageError>;

    /// Read a bounded current-format intent without guessing historical representations.
    ///
    /// # Errors
    /// Returns errors for missing, damaged, or unsupported intents.
    fn provisioning_intent(
        &self,
    ) -> Result<crate::operations::AuthProvisioningIntent, CustodyStorageError>;

    /// Clear a fence only after caller-verified publication or source reconciliation.
    ///
    /// # Errors
    /// Returns storage errors; failed removal must not be treated as acknowledged completion.
    /// Verify that no remote refresh has an unresolved outcome.
    ///
    /// # Errors
    /// Unsupported storage fails closed rather than admitting a remote exchange.
    fn ensure_no_refresh(&self) -> Result<(), CustodyStorageError> {
        Err(CustodyStorageError::MaintenanceRequired)
    }

    /// Durably admit exactly one profile-bound remote exchange.
    ///
    /// # Errors
    /// Rejects an existing fence, unsupported storage, and uncertain publication.
    fn begin_refresh(&self, _intent: &AuthRefreshIntent) -> Result<(), CustodyStorageError> {
        Err(CustodyStorageError::MaintenanceRequired)
    }

    /// Read the unresolved refresh identity without clearing or replaying it.
    ///
    /// # Errors
    /// Rejects missing, damaged, or unsupported state.
    fn refresh_intent(&self) -> Result<AuthRefreshIntent, CustodyStorageError> {
        Err(CustodyStorageError::MaintenanceRequired)
    }

    /// Clear only the matching fence after durable rotation or owner-verified re-enrollment.
    ///
    /// # Errors
    /// Rejects stale attempts; failed durability must block further use by this owner.
    fn finish_refresh(&self, _intent: &AuthRefreshIntent) -> Result<(), CustodyStorageError> {
        Err(CustodyStorageError::MaintenanceRequired)
    }

    /// Clear the device-provisioning fence after verified completion.
    ///
    /// # Errors
    /// Failed removal or durability remains unresolved.
    fn finish_provisioning(&self) -> Result<(), CustodyStorageError>;
}

/// Non-secret, owner-protected durable refresh fence. Device provisioning is independent.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthRefreshIntent {
    /// Independently versioned on-disk record.
    pub version: u16,
    /// Opaque random identity; never log it.
    pub attempt: String,
    /// Hash of the canonical custody/profile ownership binding.
    pub profile_binding: String,
}

impl AuthRefreshIntent {
    fn validate(&self) -> Result<(), CustodyStorageError> {
        if self.version != 1
            || self.attempt.len() != 64
            || self.profile_binding.len() != 64
            || !self.attempt.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !self
                .profile_binding
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        Ok(())
    }
}

/// An exclusively owned private custody directory containing only encrypted vault data.
///
/// Callers must control the directory's ancestors and exclude hostile same-user filesystem
/// mutation. The retained directory and lock confine operations after acquisition. This is
/// not an import API: ordinary sshenv files are rejected, never converted or overwritten.
#[derive(Debug)]
pub struct CredentialCustodyStorage {
    directory: Dir,
    _lock: std::fs::File,
    refresh_poisoned: std::cell::Cell<bool>,
}

const OWNER_LOCK: &str = "owner.lock";
const PENDING: &str = "pending";
const CUSTODY: &str = "custody";
const PROVISIONING: &str = "provisioning-v1";
const REFRESH: &str = "refresh-v1";

fn entry(name: &str) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(name)
}

impl CredentialCustodyStorage {
    /// Acquire an existing private custody directory without repair or initialization.
    ///
    /// # Errors
    /// Returns ownership, representation, or I/O errors, preserving existing state.
    pub fn open(path: &Path) -> Result<Self, CustodyStorageError> {
        let store = Self::acquire(path, false)?;
        store.read()?;
        Ok(store)
    }

    /// Create a fresh private directory and publish validated encrypted vault bytes.
    ///
    /// # Errors
    /// Rejects existing/relative paths or invalid vault bytes. Failed initialization is
    /// preserved for explicit maintenance, not silently retried.
    pub fn create(path: &Path, ciphertext: &[u8]) -> Result<Self, CustodyStorageError> {
        validate(ciphertext)?;
        if !path.is_absolute() {
            return Err(CustodyStorageError::Ownership);
        }
        bcode_confined_fs::create_private_dir(path).map_err(CustodyStorageError::Io)?;
        let store = Self::acquire(path, true)?;
        store.publish(ciphertext)?;
        // The directory entry itself must survive a crash before remote enrollment is admitted.
        let parent = path.parent().ok_or(CustodyStorageError::Ownership)?;
        bcode_confined_fs::sync_directory_path(parent).map_err(CustodyStorageError::Io)?;
        Ok(store)
    }

    /// Acquire durable refresh coordination for an existing native vault, without copying
    /// its ciphertext or changing its storage format. The caller controls the parent.
    /// Durable refresh coordination is acquired only for authorized credential operations.
    /// No vault conversion or credential materialization is performed by this coordinator.
    /// Errors preserve existing state for explicit maintenance.
    pub(crate) fn native_coordinator(vault: &Path) -> Result<Self, CustodyStorageError> {
        if !vault.is_absolute()
            || std::fs::symlink_metadata(vault)
                .map_err(CustodyStorageError::Io)?
                .file_type()
                .is_symlink()
        {
            return Err(CustodyStorageError::Ownership);
        }
        let mut name = vault.as_os_str().to_os_string();
        name.push(".refresh");
        let path = std::path::PathBuf::from(name);
        match bcode_confined_fs::create_private_dir(&path) {
            Ok(()) => {
                let store = Self::acquire(&path, true)?;
                store.directory.sync().map_err(CustodyStorageError::Io)?;
                bcode_confined_fs::sync_directory_path(
                    path.parent().ok_or(CustodyStorageError::Ownership)?,
                )
                .map_err(CustodyStorageError::Io)?;
                Ok(store)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::acquire(&path, false)
            }
            Err(error) => Err(CustodyStorageError::Io(error)),
        }
    }

    fn acquire(path: &Path, create: bool) -> Result<Self, CustodyStorageError> {
        if !path.is_absolute() {
            return Err(CustodyStorageError::Ownership);
        }
        let directory = Dir::open(path).map_err(CustodyStorageError::Io)?;
        if !directory.is_private().map_err(CustodyStorageError::Io)? {
            return Err(CustodyStorageError::Ownership);
        }
        let lock = directory
            .open_file(
                entry(OWNER_LOCK),
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .private(true)
                    .creation(if create {
                        Creation::New
                    } else {
                        Creation::Existing
                    }),
            )
            .map_err(CustodyStorageError::Io)?;
        lock.try_lock()
            .map_err(|_| CustodyStorageError::Ownership)?;
        let store = Self {
            directory,
            _lock: lock,
            refresh_poisoned: std::cell::Cell::new(false),
        };
        // A pending publication cannot be guessed to have succeeded or failed.
        store.ensure_absent(PENDING)?;
        Ok(store)
    }

    fn open_entry(&self, name: &str) -> std::io::Result<std::fs::File> {
        self.directory
            .open_file(entry(name), OpenOptions::new().read(true))
    }

    fn create_entry(&self, name: &str) -> Result<std::fs::File, CustodyStorageError> {
        self.directory
            .open_file(
                entry(name),
                OpenOptions::new()
                    .write(true)
                    .private(true)
                    .creation(Creation::New),
            )
            .map_err(CustodyStorageError::Io)
    }

    /// Only a definitively missing marker is absent; any other observation is ambiguous.
    fn ensure_absent(&self, name: &str) -> Result<(), CustodyStorageError> {
        match self.open_entry(name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(CustodyStorageError::MaintenanceRequired),
        }
    }

    /// Read bounded encrypted vault bytes; never unlock, repair, or select another location.
    ///
    /// # Errors
    /// Returns storage or maintenance errors for missing/unsupported/damaged data.
    pub fn read(&self) -> Result<Vec<u8>, CustodyStorageError> {
        if self.refresh_poisoned.get() {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        self.ensure_absent(PENDING)?;
        let file = self.open_entry(CUSTODY).map_err(CustodyStorageError::Io)?;
        let mut bytes = Vec::new();
        file.take((MAX_BYTES + MAGIC.len() + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(CustodyStorageError::Io)?;
        let ciphertext = bytes
            .strip_prefix(MAGIC)
            .ok_or(CustodyStorageError::MaintenanceRequired)?;
        validate(ciphertext)?;
        Ok(ciphertext.to_vec())
    }

    /// Compare current ciphertext and publish its replacement under retained ownership.
    ///
    /// # Errors
    /// Rejects stale expected bytes, invalid replacements, or interrupted publication. An
    /// error after rename is uncertain: read state before retrying, never assume rollback.
    pub fn compare_and_publish(
        &mut self,
        expected: &[u8],
        ciphertext: &[u8],
    ) -> Result<(), CustodyStorageError> {
        validate(ciphertext)?;
        if self.read()? != expected {
            return Err(CustodyStorageError::Conflict);
        }
        self.publish(ciphertext)
    }

    /// Reject unresolved external provisioning before any further credential mutation.
    pub(crate) fn ensure_no_provisioning(&self) -> Result<(), CustodyStorageError> {
        self.ensure_absent(PROVISIONING)
    }

    /// Durably fence external provisioning before dispatch. Any failure preserves the fence.
    pub(crate) fn begin_provisioning(
        &self,
        intent: &crate::operations::AuthProvisioningIntent,
    ) -> Result<(), CustodyStorageError> {
        self.ensure_no_provisioning()?;
        let bytes =
            serde_json::to_vec(intent).map_err(|_| CustodyStorageError::MaintenanceRequired)?;
        if intent.version != 2
            || intent.source.is_empty()
            || intent.operation.is_empty()
            || bytes.len() > 65536
        {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        let mut intent = self.create_entry(PROVISIONING)?;
        intent
            .write_all(&bytes)
            .and_then(|()| intent.sync_all())
            .map_err(CustodyStorageError::Io)?;
        self.directory.sync().map_err(CustodyStorageError::Io)
    }

    /// Read a bounded current-format intent; legacy markers are never guessed.
    pub(crate) fn provisioning_intent(
        &self,
    ) -> Result<crate::operations::AuthProvisioningIntent, CustodyStorageError> {
        let file = self
            .open_entry(PROVISIONING)
            .map_err(CustodyStorageError::Io)?;
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .map_err(CustodyStorageError::Io)?;
        if bytes.len() > 65536 {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        let intent: crate::operations::AuthProvisioningIntent =
            serde_json::from_slice(&bytes).map_err(|_| CustodyStorageError::MaintenanceRequired)?;
        if intent.version != 2 || intent.source.is_empty() || intent.operation.is_empty() {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        Ok(intent)
    }

    /// Clear the fence only after verified factor binding and durable custody publication.
    pub(crate) fn finish_provisioning(&self) -> Result<(), CustodyStorageError> {
        self.directory
            .remove_file(entry(PROVISIONING))
            .map_err(CustodyStorageError::Io)?;
        self.directory.sync().map_err(CustodyStorageError::Io)
    }

    fn publish(&self, ciphertext: &[u8]) -> Result<(), CustodyStorageError> {
        let mut pending = self.create_entry(PENDING)?;
        pending
            .write_all(MAGIC)
            .and_then(|()| pending.write_all(ciphertext))
            .and_then(|()| pending.sync_all())
            .map_err(CustodyStorageError::Io)?;
        // Windows cannot rename over a file that is still open without delete sharing.
        drop(pending);
        self.directory
            .rename(
                entry(PENDING),
                &self.directory,
                entry(CUSTODY),
                RenameMode::Replace,
            )
            .map_err(CustodyStorageError::Io)?;
        self.directory.sync().map_err(CustodyStorageError::Io)
    }
}

impl AuthCustodyStorage for CredentialCustodyStorage {
    fn read(&self) -> Result<Vec<u8>, CustodyStorageError> {
        Self::read(self)
    }

    fn compare_and_publish(
        &mut self,
        expected: &[u8],
        ciphertext: &[u8],
    ) -> Result<(), CustodyStorageError> {
        Self::compare_and_publish(self, expected, ciphertext)
    }

    fn ensure_no_provisioning(&self) -> Result<(), CustodyStorageError> {
        Self::ensure_no_provisioning(self)
    }

    fn begin_provisioning(
        &self,
        intent: &crate::operations::AuthProvisioningIntent,
    ) -> Result<(), CustodyStorageError> {
        Self::begin_provisioning(self, intent)
    }

    fn provisioning_intent(
        &self,
    ) -> Result<crate::operations::AuthProvisioningIntent, CustodyStorageError> {
        Self::provisioning_intent(self)
    }

    fn ensure_no_refresh(&self) -> Result<(), CustodyStorageError> {
        if self.refresh_poisoned.get() {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        self.ensure_absent(REFRESH)
    }

    fn begin_refresh(&self, intent: &AuthRefreshIntent) -> Result<(), CustodyStorageError> {
        intent.validate()?;
        self.ensure_no_refresh()?;
        self.ensure_no_provisioning()?;
        let bytes =
            serde_json::to_vec(intent).map_err(|_| CustodyStorageError::MaintenanceRequired)?;
        let mut file = self.create_entry(REFRESH)?;
        file.write_all(&bytes).map_err(CustodyStorageError::Io)?;
        file.sync_all().map_err(CustodyStorageError::Io)?;
        self.directory.sync().map_err(CustodyStorageError::Io)
    }

    fn refresh_intent(&self) -> Result<AuthRefreshIntent, CustodyStorageError> {
        if self.refresh_poisoned.get() {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        let file = self.open_entry(REFRESH).map_err(CustodyStorageError::Io)?;
        let mut bytes = Vec::new();
        file.take(1025)
            .read_to_end(&mut bytes)
            .map_err(CustodyStorageError::Io)?;
        if bytes.len() > 1024 {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        let intent: AuthRefreshIntent =
            serde_json::from_slice(&bytes).map_err(|_| CustodyStorageError::MaintenanceRequired)?;
        intent.validate()?;
        Ok(intent)
    }

    fn finish_refresh(&self, intent: &AuthRefreshIntent) -> Result<(), CustodyStorageError> {
        if self.refresh_intent()? != *intent {
            return Err(CustodyStorageError::Conflict);
        }
        self.directory
            .remove_file(entry(REFRESH))
            .map_err(CustodyStorageError::Io)?;
        if let Err(error) = self.directory.sync() {
            self.refresh_poisoned.set(true);
            return Err(CustodyStorageError::Io(error));
        }
        Ok(())
    }

    fn finish_provisioning(&self) -> Result<(), CustodyStorageError> {
        Self::finish_provisioning(self)
    }
}

fn validate(ciphertext: &[u8]) -> Result<(), CustodyStorageError> {
    if ciphertext.len() > MAX_BYTES {
        return Err(CustodyStorageError::MaintenanceRequired);
    }
    sshenv_vault::Vault::decode_ciphertext(ciphertext)
        .map_err(|_| CustodyStorageError::MaintenanceRequired)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertain_publication_blocks_reads_without_clearing_refresh() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(not(feature = "custody-simulation"))]
        let public =
            crate::security::ensure_vault_recipient_key(&temp.path().join("source")).unwrap();
        #[cfg(feature = "custody-simulation")]
        let public = "sim-age:uncertain-owner".to_owned();
        let (vault, key) = sshenv_vault::Vault::create(&public).unwrap();
        let mut ciphertext = Vec::new();
        vault
            .save_with_storage(&key, |bytes, _| {
                ciphertext = bytes.to_vec();
                Ok(())
            })
            .unwrap();
        let path = temp.path().join("owned");
        let mut storage = CredentialCustodyStorage::create(&path, &ciphertext).unwrap();
        let intent = AuthRefreshIntent {
            version: 1,
            attempt: "a".repeat(64),
            profile_binding: "b".repeat(64),
        };
        storage.begin_refresh(&intent).unwrap();
        // A crash/interrupted writer can leave this ownership-confined publication marker.
        std::fs::write(path.join("pending"), b"interrupted").unwrap();
        assert!(
            storage
                .compare_and_publish(&ciphertext, &ciphertext)
                .is_err()
        );
        assert!(storage.read().is_err());
        assert!(storage.ensure_no_refresh().is_err());
        assert!(storage.refresh_intent().unwrap() == intent);
        drop(storage);
        assert!(CredentialCustodyStorage::open(&path).is_err());
        assert!(path.join("refresh-v1").exists());
        assert_eq!(
            std::fs::read(path.join("custody")).unwrap()[MAGIC.len()..],
            ciphertext
        );
    }

    #[test]
    fn encrypted_owner_conflicts_release_and_incompatible_envelope() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(not(feature = "custody-simulation"))]
        let public =
            crate::security::ensure_vault_recipient_key(&temp.path().join("source")).unwrap();
        #[cfg(feature = "custody-simulation")]
        let public = "sim-age:storage-owner".to_owned();
        let (vault, key) = sshenv_vault::Vault::create(&public).unwrap();
        let mut ciphertext = Vec::new();
        vault
            .save_with_storage(&key, |bytes, _| {
                ciphertext = bytes.to_vec();
                Ok(())
            })
            .unwrap();
        let path = temp.path().join("owned");
        let mut storage = CredentialCustodyStorage::create(&path, &ciphertext).unwrap();
        assert!(CredentialCustodyStorage::open(&path).is_err());
        assert!(sshenv_vault::Vault::load_ciphertext(&path.join("custody")).is_err());
        assert!(matches!(
            storage.compare_and_publish(b"stale", &ciphertext),
            Err(CustodyStorageError::Conflict)
        ));
        assert_eq!(storage.read().unwrap(), ciphertext);
        storage
            .compare_and_publish(&ciphertext, &ciphertext)
            .unwrap();
        let refresh = AuthRefreshIntent {
            version: 1,
            attempt: "a".repeat(64),
            profile_binding: "b".repeat(64),
        };
        storage.begin_refresh(&refresh).unwrap();
        assert!(storage.ensure_no_refresh().is_err());
        assert!(storage.begin_refresh(&refresh).is_err());
        let stale = AuthRefreshIntent {
            attempt: "c".repeat(64),
            ..refresh.clone()
        };
        assert!(storage.finish_refresh(&stale).is_err());
        drop(storage);
        let storage = CredentialCustodyStorage::open(&path).unwrap();
        assert!(storage.refresh_intent().unwrap() == refresh);
        assert!(storage.ensure_no_refresh().is_err());
        storage.finish_refresh(&refresh).unwrap();
        storage.ensure_no_refresh().unwrap();
        let intent = crate::operations::AuthProvisioningIntent {
            version: 2,
            source: "test".into(),
            operation: "attempt-1".into(),
            profile_binding: "test-binding".into(),
        };
        storage.begin_provisioning(&intent).unwrap();
        drop(storage);
        let reopened = CredentialCustodyStorage::open(&path).unwrap();
        assert!(matches!(
            reopened.ensure_no_provisioning(),
            Err(CustodyStorageError::MaintenanceRequired)
        ));
        assert!(reopened.provisioning_intent().unwrap() == intent);
        assert!(reopened.begin_provisioning(&intent).is_err());
        reopened.finish_provisioning().unwrap();
        reopened.ensure_no_provisioning().unwrap();
        assert_eq!(reopened.read().unwrap(), ciphertext);
        drop(reopened);
        std::fs::write(path.join("custody"), &ciphertext).unwrap();
        assert!(matches!(
            CredentialCustodyStorage::open(&path),
            Err(CustodyStorageError::MaintenanceRequired)
        ));
        assert_eq!(std::fs::read(path.join("custody")).unwrap(), ciphertext);
    }
}
