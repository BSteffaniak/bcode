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
    _file: File,
    _execution_admission: File,
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
        let _lifetime = ExecutionLifetime::publish(root, record, self.file.try_clone()?)?;
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
        .join(format!("{digest:x}.json"))
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
        Self::publish(root, record, execution_admission)
    }

    fn publish(root: &Path, record: &DaemonRecord, execution_admission: File) -> io::Result<Self> {
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
        file.try_lock().map_err(|_| invalid())?;
        let bytes = serde_json::to_vec(&evidence).map_err(|_| invalid())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid());
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        File::open(root.join("daemon-execution-lifetimes"))?.sync_all()?;
        Ok(Self {
            _file: file,
            _execution_admission: execution_admission,
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
    match file.try_lock() {
        Ok(()) => Ok(ExecutionLifetimeStatus::Released),
        Err(fs::TryLockError::WouldBlock) => Ok(ExecutionLifetimeStatus::Live),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}
