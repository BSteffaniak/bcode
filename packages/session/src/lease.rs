//! Cross-process session and catalog lease primitives.

use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Serializable metadata describing the daemon that owns a session lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLeaseOwner {
    /// Metadata schema version.
    pub schema_version: u32,
    /// Session protected by this lease.
    pub session_id: SessionId,
    /// Unique token for this lease acquisition.
    pub lease_token: String,
    /// Owning process id.
    pub pid: u32,
    /// Bcode daemon namespace, when known.
    pub daemon_namespace: Option<String>,
    /// Bcode build fingerprint, when known.
    pub build_fingerprint: Option<String>,
    /// Bcode IPC protocol version, when known.
    pub protocol_version: Option<u32>,
    /// Owning daemon endpoint, when known.
    pub endpoint: Option<String>,
    /// Executable that acquired the lease, when known.
    pub executable_path: Option<PathBuf>,
    /// Daemon instance id, when known.
    pub daemon_instance_id: Option<String>,
    /// Lease acquisition time in Unix milliseconds.
    pub acquired_at_ms: u64,
    /// Latest owner heartbeat/update time in Unix milliseconds.
    pub last_seen_ms: u64,
}

impl SessionLeaseOwner {
    fn new(session_id: SessionId, context: &SessionLeaseOwnerContext) -> Self {
        let now = unix_time_millis();
        Self {
            schema_version: 1,
            session_id,
            lease_token: format!("{}-{now}-{session_id}", std::process::id()),
            pid: std::process::id(),
            daemon_namespace: context.daemon_namespace.clone(),
            build_fingerprint: context.build_fingerprint.clone(),
            protocol_version: context.protocol_version,
            endpoint: context.endpoint.clone(),
            executable_path: context
                .executable_path
                .clone()
                .or_else(|| std::env::current_exe().ok()),
            daemon_instance_id: context.daemon_instance_id.clone(),
            acquired_at_ms: now,
            last_seen_ms: now,
        }
    }
}

/// Optional daemon identity to write into session lease owner metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionLeaseOwnerContext {
    /// Bcode daemon namespace.
    pub daemon_namespace: Option<String>,
    /// Bcode build fingerprint.
    pub build_fingerprint: Option<String>,
    /// Bcode IPC protocol version.
    pub protocol_version: Option<u32>,
    /// Bcode IPC endpoint description.
    pub endpoint: Option<String>,
    /// Daemon executable path.
    pub executable_path: Option<PathBuf>,
    /// Daemon instance id.
    pub daemon_instance_id: Option<String>,
}

/// Errors returned while acquiring or releasing cross-process leases.
#[derive(Debug, Error)]
pub enum SessionLeaseError {
    /// Filesystem operation failed.
    #[error("session lease I/O error at {}: {source}", path.display())]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Original I/O error.
        source: io::Error,
    },
    /// Owner metadata could not be serialized.
    #[error("session lease metadata serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    /// Another daemon currently owns the session lease.
    #[error("session {session_id} is owned by another daemon{owner_summary}")]
    OwnedByOtherDaemon {
        /// Session whose lease could not be acquired.
        session_id: SessionId,
        /// Best-effort owner metadata.
        owner: Option<Box<SessionLeaseOwner>>,
        /// Human-readable owner summary.
        owner_summary: String,
    },
    /// Current platform does not support Bcode's session lease mechanism.
    #[error("session leases are unsupported on this platform")]
    Unsupported,
}

/// Held exclusive ownership for one session.
#[derive(Debug)]
pub struct SessionLeaseGuard {
    file: File,
    _lock_path: PathBuf,
    owner_path: PathBuf,
    owner: SessionLeaseOwner,
}

impl SessionLeaseGuard {
    /// Return owner metadata for this held lease.
    #[must_use]
    pub const fn owner(&self) -> &SessionLeaseOwner {
        &self.owner
    }
}

impl Drop for SessionLeaseGuard {
    fn drop(&mut self) {
        remove_owner_metadata_if_token_matches(&self.owner_path, &self.owner.lease_token);
        let _ = unlock_file(&self.file);
    }
}

/// Held exclusive access to the global catalog database.
#[derive(Debug)]
pub struct CatalogLockGuard {
    file: File,
}

impl Drop for CatalogLockGuard {
    fn drop(&mut self) {
        let _ = unlock_file(&self.file);
    }
}

/// Acquire exclusive ownership for a session using a kernel-backed file lock.
///
/// # Errors
///
/// Returns an error if the lock file cannot be opened, another daemon owns the session,
/// owner metadata cannot be written, or the platform does not support file leases.
pub fn acquire_session_lease(
    root: &Path,
    session_id: SessionId,
    context: &SessionLeaseOwnerContext,
) -> Result<SessionLeaseGuard, SessionLeaseError> {
    let lock_path = session_lock_path(root, session_id);
    let owner_path = session_owner_path(root, session_id);
    let file = open_lock_file(&lock_path)?;
    match try_lock_file_exclusive(&file) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            let owner = read_owner_metadata(&owner_path).map(Box::new);
            let owner_summary = owner
                .as_deref()
                .map_or_else(String::new, |owner| format!(": {}", format_owner(owner)));
            return Err(SessionLeaseError::OwnedByOtherDaemon {
                session_id,
                owner,
                owner_summary,
            });
        }
        Err(source) => {
            return Err(SessionLeaseError::Io {
                path: lock_path,
                source,
            });
        }
    }

    let owner = SessionLeaseOwner::new(session_id, context);
    write_owner_metadata(&owner_path, &owner)?;
    Ok(SessionLeaseGuard {
        file,
        _lock_path: lock_path,
        owner_path,
        owner,
    })
}

/// Acquire exclusive short-lived access to the global catalog database.
///
/// # Errors
///
/// Returns an error if the catalog lock file cannot be opened or locked.
pub fn acquire_catalog_lock(root: &Path) -> Result<CatalogLockGuard, SessionLeaseError> {
    let lock_path = root.join("catalog.lock");
    let file = open_lock_file(&lock_path)?;
    lock_file_exclusive(&file).map_err(|source| SessionLeaseError::Io {
        path: lock_path,
        source,
    })?;
    Ok(CatalogLockGuard { file })
}

fn session_lock_path(root: &Path, session_id: SessionId) -> PathBuf {
    root.join("locks").join(format!("{session_id}.lock"))
}

fn session_owner_path(root: &Path, session_id: SessionId) -> PathBuf {
    root.join("leases").join(format!("{session_id}.json"))
}

fn open_lock_file(path: &Path) -> Result<File, SessionLeaseError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| SessionLeaseError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| SessionLeaseError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn write_owner_metadata(path: &Path, owner: &SessionLeaseOwner) -> Result<(), SessionLeaseError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| SessionLeaseError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let temp_path = path.with_extension(format!("json.tmp-{}", owner.lease_token));
    let contents = serde_json::to_vec_pretty(owner)?;
    fs::write(&temp_path, contents).map_err(|source| SessionLeaseError::Io {
        path: temp_path.clone(),
        source,
    })?;
    fs::rename(&temp_path, path).map_err(|source| SessionLeaseError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_owner_metadata(path: &Path) -> Option<SessionLeaseOwner> {
    let contents = fs::read(path).ok()?;
    serde_json::from_slice(&contents).ok()
}

fn remove_owner_metadata_if_token_matches(path: &Path, token: &str) {
    let Some(owner) = read_owner_metadata(path) else {
        return;
    };
    if owner.lease_token == token {
        let _ = fs::remove_file(path);
    }
}

fn format_owner(owner: &SessionLeaseOwner) -> String {
    let mut parts = vec![format!("pid {}", owner.pid)];
    if let Some(namespace) = &owner.daemon_namespace {
        parts.push(format!("namespace {namespace}"));
    }
    if let Some(fingerprint) = &owner.build_fingerprint {
        parts.push(format!("build {fingerprint}"));
    }
    if let Some(endpoint) = &owner.endpoint {
        parts.push(format!("endpoint {endpoint}"));
    }
    parts.join(", ")
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(unix)]
fn try_lock_file_exclusive(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;

    let result = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn try_lock_file_exclusive(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        SessionLeaseError::Unsupported.to_string(),
    ))
}

#[cfg(unix)]
fn lock_file_exclusive(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    const LOCK_EX: i32 = 2;

    let result = unsafe { flock(file.as_raw_fd(), LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_file_exclusive(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        SessionLeaseError::Unsupported.to_string(),
    ))
}

#[cfg(unix)]
fn unlock_file(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    const LOCK_UN: i32 = 8;

    let result = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn unlock_file(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}
