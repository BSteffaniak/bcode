//! Explicit storage for encrypted credential custody, separate from subscription state.

use std::io::{Read as _, Write as _};
use std::path::Path;

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
        drop(storage);
        let reopened = CredentialCustodyStorage::open(&path).unwrap();
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
