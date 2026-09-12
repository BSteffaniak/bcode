//! Explicit storage for encrypted credential custody, separate from subscription state.

use std::io::{Read as _, Write as _};
use std::path::Path;

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
    fn finish_provisioning(&self) -> Result<(), CustodyStorageError>;
}

/// An exclusively owned Unix custody directory containing only encrypted vault data.
///
/// Callers must control the directory's ancestors and exclude hostile same-user filesystem
/// mutation. The retained directory and lock confine operations after acquisition. This is
/// not an import API: ordinary sshenv files are rejected, never converted or overwritten.
#[cfg(unix)]
#[derive(Debug)]
pub struct CredentialCustodyStorage {
    directory: std::fs::File,
    _lock: std::fs::File,
}

#[cfg(unix)]
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
        use std::os::unix::fs::DirBuilderExt as _;
        validate(ciphertext)?;
        if !path.is_absolute() {
            return Err(CustodyStorageError::Ownership);
        }
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(CustodyStorageError::Io)?;
        let store = Self::acquire(path, true)?;
        store.publish(ciphertext)?;
        Ok(store)
    }

    fn acquire(path: &Path, create: bool) -> Result<Self, CustodyStorageError> {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        if !path.is_absolute() {
            return Err(CustodyStorageError::Ownership);
        }
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(CustodyStorageError::Io)?;
        let metadata = directory.metadata().map_err(CustodyStorageError::Io)?;
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(CustodyStorageError::Ownership);
        }
        let lock = crate::store::open_owned_entry(
            &directory,
            c"owner.lock",
            libc::O_RDWR
                | if create {
                    libc::O_CREAT | libc::O_EXCL
                } else {
                    0
                },
        )
        .map_err(CustodyStorageError::Io)?;
        lock.try_lock()
            .map_err(|_| CustodyStorageError::Ownership)?;
        // A pending publication cannot be guessed to have succeeded or failed.
        match crate::store::open_owned_entry(&directory, c"pending", libc::O_RDONLY) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(CustodyStorageError::MaintenanceRequired),
        }
        Ok(Self {
            directory,
            _lock: lock,
        })
    }

    /// Read bounded encrypted vault bytes; never unlock, repair, or select another location.
    ///
    /// # Errors
    /// Returns storage or maintenance errors for missing/unsupported/damaged data.
    pub fn read(&self) -> Result<Vec<u8>, CustodyStorageError> {
        let file = crate::store::open_owned_entry(&self.directory, c"custody", libc::O_RDONLY)
            .map_err(CustodyStorageError::Io)?;
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
        match crate::store::open_owned_entry(&self.directory, c"provisioning-v1", libc::O_RDONLY) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(CustodyStorageError::MaintenanceRequired),
        }
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
        let mut intent = crate::store::open_owned_entry(
            &self.directory,
            c"provisioning-v1",
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(CustodyStorageError::Io)?;
        intent
            .write_all(&bytes)
            .and_then(|()| intent.sync_all())
            .map_err(CustodyStorageError::Io)?;
        self.directory.sync_all().map_err(CustodyStorageError::Io)
    }

    /// Read a bounded current-format intent; legacy markers are never guessed.
    pub(crate) fn provisioning_intent(
        &self,
    ) -> Result<crate::operations::AuthProvisioningIntent, CustodyStorageError> {
        let file =
            crate::store::open_owned_entry(&self.directory, c"provisioning-v1", libc::O_RDONLY)
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
        use std::os::fd::AsRawFd as _;
        // SAFETY: the static relative name and retained directory descriptor are valid.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), c"provisioning-v1".as_ptr(), 0) }
            != 0
        {
            return Err(CustodyStorageError::Io(std::io::Error::last_os_error()));
        }
        self.directory.sync_all().map_err(CustodyStorageError::Io)
    }

    fn publish(&self, ciphertext: &[u8]) -> Result<(), CustodyStorageError> {
        use std::os::fd::AsRawFd as _;
        let mut pending = crate::store::open_owned_entry(
            &self.directory,
            c"pending",
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(CustodyStorageError::Io)?;
        pending
            .write_all(MAGIC)
            .and_then(|()| pending.write_all(ciphertext))
            .and_then(|()| pending.sync_all())
            .map_err(CustodyStorageError::Io)?;
        // SAFETY: both static C names are relative to the live retained directory descriptor.
        if unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                c"pending".as_ptr(),
                self.directory.as_raw_fd(),
                c"custody".as_ptr(),
            )
        } != 0
        {
            return Err(CustodyStorageError::Io(std::io::Error::last_os_error()));
        }
        self.directory.sync_all().map_err(CustodyStorageError::Io)
    }
}

#[cfg(unix)]
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
    fn encrypted_owner_conflicts_release_and_incompatible_envelope() {
        let temp = tempfile::tempdir().unwrap();
        let public =
            crate::security::ensure_vault_recipient_key(&temp.path().join("source")).unwrap();
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
