//! Durable, per-instance execution lifetime evidence, independent of discovery cleanup.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::DaemonRecord;

const VERSION: u32 = 1;

#[cfg(test)]
mod tests;
const MAX_BYTES: u64 = 4096;

#[derive(Serialize, Deserialize)]
struct Evidence {
    version: u32,
    instance_id: String,
    artifact_id: String,
    state_location_id: String,
}

/// An exact daemon instance's execution lifetime, separate from its disposable registry entry.
///
/// Retain this guard until the final execution-capable server reference is released. Never
/// recreate an instance's evidence file: acquiring an existing unlocked file is observation,
/// not permission to execute as that instance. Files are intentionally retained after release.
#[derive(Debug)]
pub struct ExecutionLifetime {
    file: File,
    execution_admission: File,
    owns_admission_lock: bool,
    owner_pid: u32,
}

impl Drop for ExecutionLifetime {
    fn drop(&mut self) {
        // Forked children can temporarily retain the same open file description
        // before exec. Closing only our descriptor would leave its locks live.
        // Only the publishing process may explicitly relinquish this authority.
        // Maintenance coordinators borrow an admission descriptor: unlocking
        // that clone would prematurely release the maintenance owner's fence.
        if self.owner_pid == std::process::id() {
            let _ = self.file.unlock();
            if self.owns_admission_lock {
                let _ = self.execution_admission.unlock();
            }
        }
    }
}

/// Exclusive state-location execution admission for explicit offline maintenance.
/// All clients and daemons must first be upgraded to the execution-fence protocol.
#[derive(Debug)]
pub struct ExecutionMaintenance {
    file: File,
    root: PathBuf,
}

fn admission_file(root: &Path) -> io::Result<File> {
    let root = directory(root, true)?;
    let path = root
        .join("daemon-execution-lifetimes")
        .join("admission.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid());
    }
    Ok(file)
}

impl ExecutionMaintenance {
    /// Fence startup and execution in this state location during offline maintenance.
    ///
    /// # Errors
    /// Rejects active upgraded execution owners, unsafe paths and IO failures.
    /// Publish a released maintenance coordinator after a fenced offline transfer.
    /// The caller must retain this exclusive guard until the store transaction commits.
    ///
    /// # Errors
    /// Rejects reused identities, unsupported paths or persistence failures.
    pub fn publish_coordinator(&self, root: &Path, record: &DaemonRecord) -> io::Result<()> {
        if root.canonicalize()? != self.root {
            return Err(invalid());
        }
        let _lifetime = ExecutionLifetime::publish(root, record, self.file.try_clone()?, false)?;
        Ok(())
    }

    /// Fence execution and startup for explicit offline maintenance.
    ///
    /// # Errors
    /// Rejects live upgraded owners, unsafe paths and IO failures.
    pub fn acquire(root: &Path) -> io::Result<Self> {
        let file = admission_file(root)?;
        file.try_lock().map_err(io::Error::from)?;
        Ok(Self {
            file,
            root: root.canonicalize()?,
        })
    }
}

/// Result of a bounded, non-mutating lifetime observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionLifetimeStatus {
    /// The exact instance still holds its execution lifetime lock.
    Live,
    /// The exact instance's published lifetime lock has been released.
    Released,
    /// This instance predates durable lifetime publication or its evidence is missing.
    Missing,
    /// No supported, identity-matching evidence is available.
    Unverifiable,
}

fn evidence_path(root: &Path, instance_id: &str) -> PathBuf {
    let digest = Sha256::digest(instance_id.as_bytes());
    root.join("daemon-execution-lifetimes")
        .join(format!("{}.json", hex::encode(digest)))
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "daemon execution lifetime evidence is unverifiable",
    )
}

fn directory(root: &Path, create: bool) -> io::Result<PathBuf> {
    let root = root.canonicalize()?;
    let path = root.join("daemon-execution-lifetimes");
    if create {
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    if !fs::symlink_metadata(&path)?.is_dir()
        || path.canonicalize()?.parent() != Some(root.as_path())
    {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        // Persist the directory entry before any workflow can reference this lifetime.
        if create {
            File::open(&root)?.sync_all()?;
        }
    }
    Ok(root)
}

impl ExecutionLifetime {
    /// Publish a new instance's durable execution lifetime before admitting any work.
    ///
    /// # Errors
    /// Returns an error for missing identity, reused instance IDs, unsafe paths or IO failure.
    pub fn begin(root: &Path, record: &DaemonRecord) -> io::Result<Self> {
        let execution_admission = admission_file(root)?;
        execution_admission
            .try_lock_shared()
            .map_err(io::Error::from)?;
        Self::publish(root, record, execution_admission, true)
    }

    fn publish(
        root: &Path,
        record: &DaemonRecord,
        execution_admission: File,
        owns_admission_lock: bool,
    ) -> io::Result<Self> {
        let root = directory(root, true)?;
        let evidence = Evidence {
            version: VERSION,
            instance_id: record.instance_id.clone(),
            artifact_id: record.artifact_id.as_ref().ok_or_else(invalid)?.to_string(),
            state_location_id: record.state_location_id.clone().ok_or_else(invalid)?,
        };
        if evidence.instance_id.is_empty() || evidence.state_location_id.is_empty() {
            return Err(invalid());
        }
        let path = evidence_path(&root, &record.instance_id);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        try_lock_evidence(&file).map_err(|_| invalid())?;
        let bytes = serde_json::to_vec(&evidence).map_err(|_| invalid())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid());
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        File::open(root.join("daemon-execution-lifetimes"))?.sync_all()?;
        Ok(Self {
            file,
            execution_admission,
            owns_admission_lock,
            owner_pid: std::process::id(),
        })
    }
}

/// Observe an exact lifetime without creating, repairing or modifying evidence.
///
/// Missing, corrupt, future, or identity-mismatched evidence fails closed. A released lock is
/// positive evidence only because supported writers publish it before work, retain it through
/// all execution-capable references, and never reuse an instance identity.
#[must_use]
pub fn execution_lifetime_status(
    root: &Path,
    instance_id: &str,
    artifact_id: &str,
    state_location_id: &str,
) -> ExecutionLifetimeStatus {
    match observe(root, instance_id, artifact_id, state_location_id) {
        Ok(status) => status,
        Err(error) if error.kind() == io::ErrorKind::NotFound => ExecutionLifetimeStatus::Missing,
        Err(_) => ExecutionLifetimeStatus::Unverifiable,
    }
}

#[cfg(not(windows))]
fn try_lock_evidence(file: &File) -> Result<(), fs::TryLockError> {
    file.try_lock()
}

#[cfg(windows)]
fn try_lock_evidence(file: &File) -> Result<(), fs::TryLockError> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    // Windows locks deny reads of the locked bytes. Lock beyond the maximum
    // evidence payload, retaining exclusive lifetime fencing without hiding identity.
    let mut overlapped = windows_sys::Win32::System::IO::OVERLAPPED {
        Anonymous: windows_sys::Win32::System::IO::OVERLAPPED_0 {
            Anonymous: windows_sys::Win32::System::IO::OVERLAPPED_0_0 {
                Offset: u32::try_from(MAX_BYTES).expect("bounded evidence"),
                OffsetHigh: 0,
            },
        },
        ..Default::default()
    };
    // SAFETY: the live file handle and initialized OVERLAPPED remain valid for this
    // synchronous, nonblocking call. Closing the file releases the one-byte lock.
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &raw mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error()
        == Some(
            i32::try_from(windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION)
                .expect("Win32 error fits i32"),
        )
    {
        Err(fs::TryLockError::WouldBlock)
    } else {
        Err(fs::TryLockError::Error(error))
    }
}

fn observe(
    root: &Path,
    instance: &str,
    artifact: &str,
    location: &str,
) -> io::Result<ExecutionLifetimeStatus> {
    let root = directory(root, false)?;
    let path = evidence_path(&root, instance);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(invalid());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let mut bytes = Vec::new();
    (&mut file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid());
    }
    let evidence: Evidence = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if evidence.version != VERSION
        || evidence.instance_id != instance
        || evidence.artifact_id != artifact
        || evidence.state_location_id != location
    {
        return Err(invalid());
    }
    match try_lock_evidence(&file) {
        Ok(()) => Ok(ExecutionLifetimeStatus::Released),
        Err(fs::TryLockError::WouldBlock) => Ok(ExecutionLifetimeStatus::Live),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}
