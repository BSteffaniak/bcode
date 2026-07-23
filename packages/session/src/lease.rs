//! Cross-process session compatibility and catalog lock primitives.
//!
//! Session access guards intentionally do not provide exclusive session ownership. Bcode's UX
//! allows multiple clients and same-version daemons to attach to the same session, while the
//! database provides write serialization. These guards only prevent incompatible Bcode builds from
//! accessing the same session concurrently.

use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Durable session-storage writer contract supported by this Bcode build.
pub const CURRENT_SESSION_STORAGE_WRITER_EPOCH: u32 = 4;

/// Serializable metadata describing one process currently accessing a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLeaseOwner {
    /// Metadata schema version.
    pub schema_version: u32,
    /// Session protected by this access record.
    pub session_id: SessionId,
    /// Unique token for this access registration.
    pub lease_token: String,
    /// Accessing process id.
    pub pid: u32,
    /// Durable session-storage writer epoch supported by this process.
    #[serde(default)]
    pub storage_writer_epoch: Option<u32>,
    /// Bcode daemon namespace, when known.
    pub daemon_namespace: Option<String>,
    /// Bcode build fingerprint, when known.
    pub build_fingerprint: Option<String>,
    /// Bcode IPC protocol version, when known.
    pub protocol_version: Option<u32>,
    /// Accessing daemon endpoint, when known.
    pub endpoint: Option<String>,
    /// Executable that registered access, when known.
    pub executable_path: Option<PathBuf>,
    /// Daemon instance id, when known.
    pub daemon_instance_id: Option<String>,
    /// Access registration time in Unix milliseconds.
    pub acquired_at_ms: u64,
    /// Latest owner heartbeat/update time in Unix milliseconds.
    pub last_seen_ms: u64,
}

impl SessionLeaseOwner {
    fn new(session_id: SessionId, context: &SessionLeaseOwnerContext) -> Self {
        let now = unix_time_millis();
        Self {
            schema_version: 2,
            session_id,
            lease_token: format!("{}-{now}-{session_id}", std::process::id()),
            pid: std::process::id(),
            storage_writer_epoch: context.storage_writer_epoch,
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

/// Optional daemon identity to write into session access metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLeaseOwnerContext {
    /// Durable session-storage writer epoch.
    pub storage_writer_epoch: Option<u32>,
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

impl Default for SessionLeaseOwnerContext {
    fn default() -> Self {
        Self {
            storage_writer_epoch: Some(CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            daemon_namespace: None,
            build_fingerprint: None,
            protocol_version: None,
            endpoint: None,
            executable_path: None,
            daemon_instance_id: None,
        }
    }
}

/// Errors returned while registering or releasing cross-process session access.
#[derive(Debug, Error)]
pub enum SessionLeaseError {
    /// Filesystem operation failed.
    #[error("session access I/O error at {}: {source}", path.display())]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Original I/O error.
        source: io::Error,
    },
    /// Owner metadata could not be serialized.
    #[error("session access metadata serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    /// A mutation-capable session lease omitted its required storage writer identity.
    #[error("session {session_id} access requires an explicit storage writer epoch")]
    MissingStorageWriterEpoch {
        /// Session whose lease identity was incomplete.
        session_id: SessionId,
    },
    /// Another daemon with an incompatible storage writer currently has the session open.
    #[error("session {session_id} is open in another incompatible Bcode writer{owner_summary}")]
    OwnedByOtherDaemon {
        /// Session whose access could not be registered.
        session_id: SessionId,
        /// Best-effort incompatible owner metadata.
        owner: Option<Box<SessionLeaseOwner>>,
        /// Human-readable owner summary.
        owner_summary: String,
    },
    /// Current platform does not support Bcode's session access mechanism.
    #[error("session access guards are unsupported on this platform")]
    Unsupported,
}

/// Held compatible access registration for one session.
#[derive(Debug)]
pub struct SessionLeaseGuard {
    owner_path: PathBuf,
    owner: SessionLeaseOwner,
}

impl SessionLeaseGuard {
    /// Return owner metadata for this held access registration.
    #[must_use]
    pub const fn owner(&self) -> &SessionLeaseOwner {
        &self.owner
    }
}

impl Drop for SessionLeaseGuard {
    fn drop(&mut self) {
        remove_owner_metadata_if_token_matches(&self.owner_path, &self.owner.lease_token);
    }
}

/// Held exclusive maintenance access to one session.
#[derive(Debug)]
pub struct SessionMaintenanceGuard {
    coordinator: File,
}

impl Drop for SessionMaintenanceGuard {
    fn drop(&mut self) {
        let _ = unlock_file(&self.coordinator);
    }
}

/// Held short-lived exclusive access to one session write critical section.
#[derive(Debug)]
pub struct SessionWriteGuard {
    file: File,
    coordinator: Option<File>,
}

impl Drop for SessionWriteGuard {
    fn drop(&mut self) {
        let _ = unlock_file(&self.file);
        if let Some(coordinator) = &self.coordinator {
            let _ = unlock_file(coordinator);
        }
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

/// Return live owner metadata for one session without mutating owner records.
///
/// # Errors
///
/// Returns an error when the owner directory cannot be inspected.
pub fn active_session_owners(
    root: &Path,
    session_id: SessionId,
) -> Result<Vec<SessionLeaseOwner>, SessionLeaseError> {
    let access_dir = session_owner_dir(root, session_id);
    let Ok(entries) = fs::read_dir(&access_dir) else {
        return Ok(Vec::new());
    };
    let mut owners = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| SessionLeaseError::Io {
            path: access_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        if let Some(owner) = read_owner_metadata(&path)
            && process_is_alive(owner.pid)
        {
            owners.push(owner);
        }
    }
    owners.sort_by(|left, right| left.lease_token.cmp(&right.lease_token));
    Ok(owners)
}

/// Register compatible access for a session.
///
/// This is not an exclusive session lock. It briefly takes a coordinator lock, removes dead
/// process registrations, rejects live incompatible builds, writes this process registration, and
/// then lets the database handle read/write concurrency.
///
/// # Errors
///
/// Returns an error if the coordinator lock cannot be opened/locked, owner metadata cannot be
/// read/written, or an incompatible live build is already registered for the session.
pub fn acquire_session_lease(
    root: &Path,
    session_id: SessionId,
    context: &SessionLeaseOwnerContext,
) -> Result<SessionLeaseGuard, SessionLeaseError> {
    if context.storage_writer_epoch.is_none() {
        return Err(SessionLeaseError::MissingStorageWriterEpoch { session_id });
    }
    let lock_path = session_lock_path(root, session_id);
    let file = open_lock_file(&lock_path)?;
    lock_file_exclusive(&file).map_err(|source| SessionLeaseError::Io {
        path: lock_path.clone(),
        source,
    })?;

    let access_dir = session_owner_dir(root, session_id);
    fs::create_dir_all(&access_dir).map_err(|source| SessionLeaseError::Io {
        path: access_dir.clone(),
        source,
    })?;
    prune_dead_owner_records(&access_dir)?;

    if let Some(owner) = find_incompatible_owner(&access_dir, context)? {
        let owner_summary = format!(": {}", format_owner(&owner));
        let _ = unlock_file(&file);
        return Err(SessionLeaseError::OwnedByOtherDaemon {
            session_id,
            owner: Some(Box::new(owner)),
            owner_summary,
        });
    }

    let owner = SessionLeaseOwner::new(session_id, context);
    let owner_path = access_dir.join(format!("{}.json", owner.lease_token));
    write_owner_metadata(&owner_path, &owner)?;
    let _ = unlock_file(&file);
    Ok(SessionLeaseGuard { owner_path, owner })
}

/// Acquire exclusive maintenance access, refusing every live session owner.
///
/// The coordinator lock remains held for the guard lifetime, preventing new owners from
/// registering while an offline migration is active.
///
/// # Errors
///
/// Returns an error if locking or owner inspection fails, or if any live owner exists.
pub fn acquire_session_maintenance_guard(
    root: &Path,
    session_id: SessionId,
) -> Result<SessionMaintenanceGuard, SessionLeaseError> {
    let lock_path = session_lock_path(root, session_id);
    let coordinator = open_lock_file(&lock_path)?;
    lock_file_exclusive(&coordinator).map_err(|source| SessionLeaseError::Io {
        path: lock_path,
        source,
    })?;
    let access_dir = session_owner_dir(root, session_id);
    fs::create_dir_all(&access_dir).map_err(|source| SessionLeaseError::Io {
        path: access_dir.clone(),
        source,
    })?;
    prune_dead_owner_records(&access_dir)?;
    if let Some(owner) = first_owner(&access_dir)? {
        return Err(SessionLeaseError::OwnedByOtherDaemon {
            session_id,
            owner_summary: format!(": {}", format_owner(&owner)),
            owner: Some(Box::new(owner)),
        });
    }
    Ok(SessionMaintenanceGuard { coordinator })
}

/// Atomically replace exclusive maintenance ownership with a compatible session lease.
///
/// The maintenance coordinator remains locked while owner metadata is written, so another writer
/// cannot claim the session between migration completion and runtime ownership registration.
///
/// # Errors
///
/// Returns an error if writer identity is missing or owner metadata cannot be written.
pub fn transition_session_maintenance_to_lease(
    maintenance: SessionMaintenanceGuard,
    root: &Path,
    session_id: SessionId,
    context: &SessionLeaseOwnerContext,
) -> Result<SessionLeaseGuard, SessionLeaseError> {
    if context.storage_writer_epoch.is_none() {
        return Err(SessionLeaseError::MissingStorageWriterEpoch { session_id });
    }
    let access_dir = session_owner_dir(root, session_id);
    fs::create_dir_all(&access_dir).map_err(|source| SessionLeaseError::Io {
        path: access_dir.clone(),
        source,
    })?;
    prune_dead_owner_records(&access_dir)?;
    if let Some(owner) = first_owner(&access_dir)? {
        return Err(SessionLeaseError::OwnedByOtherDaemon {
            session_id,
            owner_summary: format!(": {}", format_owner(&owner)),
            owner: Some(Box::new(owner)),
        });
    }
    let owner = SessionLeaseOwner::new(session_id, context);
    let owner_path = access_dir.join(format!("{}.json", owner.lease_token));
    write_owner_metadata(&owner_path, &owner)?;
    drop(maintenance);
    Ok(SessionLeaseGuard { owner_path, owner })
}

fn first_owner(access_dir: &Path) -> Result<Option<SessionLeaseOwner>, SessionLeaseError> {
    for entry in fs::read_dir(access_dir).map_err(|source| SessionLeaseError::Io {
        path: access_dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| SessionLeaseError::Io {
            path: access_dir.to_path_buf(),
            source,
        })?;
        if entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            && let Some(owner) = read_owner_metadata(&entry.path())
        {
            return Ok(Some(owner));
        }
    }
    Ok(None)
}

/// Acquire exclusive short-lived access to one session write critical section.
///
/// This does not represent session ownership. It only serializes app-assigned event sequence
/// allocation across same-version daemons until sequence allocation is fully database-owned.
///
/// # Errors
///
/// Returns an error if the write lock file cannot be opened or locked.
pub fn acquire_session_write_lock(
    root: &Path,
    session_id: SessionId,
) -> Result<SessionWriteGuard, SessionLeaseError> {
    let coordinator_path = session_lock_path(root, session_id);
    let coordinator = open_lock_file(&coordinator_path)?;
    lock_file_shared(&coordinator).map_err(|source| SessionLeaseError::Io {
        path: coordinator_path,
        source,
    })?;
    acquire_session_write_lock_inner(root, session_id, Some(coordinator))
}

/// Acquire the session write lock while exclusive maintenance coordination is already held.
///
/// # Errors
///
/// Returns an error if the write lock file cannot be opened or locked.
pub fn acquire_maintenance_session_write_lock(
    _maintenance: &SessionMaintenanceGuard,
    root: &Path,
    session_id: SessionId,
) -> Result<SessionWriteGuard, SessionLeaseError> {
    acquire_session_write_lock_inner(root, session_id, None)
}

fn acquire_session_write_lock_inner(
    root: &Path,
    session_id: SessionId,
    coordinator: Option<File>,
) -> Result<SessionWriteGuard, SessionLeaseError> {
    let lock_path = session_write_lock_path(root, session_id);
    let file = open_lock_file(&lock_path)?;
    lock_file_exclusive(&file).map_err(|source| SessionLeaseError::Io {
        path: lock_path,
        source,
    })?;
    Ok(SessionWriteGuard { file, coordinator })
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

fn session_write_lock_path(root: &Path, session_id: SessionId) -> PathBuf {
    root.join("locks").join(format!("{session_id}.write.lock"))
}

fn session_owner_dir(root: &Path, session_id: SessionId) -> PathBuf {
    root.join("leases").join(session_id.to_string())
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

fn prune_dead_owner_records(access_dir: &Path) -> Result<(), SessionLeaseError> {
    for entry in fs::read_dir(access_dir).map_err(|source| SessionLeaseError::Io {
        path: access_dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| SessionLeaseError::Io {
            path: access_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(owner) = read_owner_metadata(&path) else {
            remove_file_best_effort(&path)?;
            continue;
        };
        if !process_is_alive(owner.pid) {
            remove_file_best_effort(&path)?;
        }
    }
    Ok(())
}

fn find_incompatible_owner(
    access_dir: &Path,
    context: &SessionLeaseOwnerContext,
) -> Result<Option<SessionLeaseOwner>, SessionLeaseError> {
    for entry in fs::read_dir(access_dir).map_err(|source| SessionLeaseError::Io {
        path: access_dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| SessionLeaseError::Io {
            path: access_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(owner) = read_owner_metadata(&path) else {
            continue;
        };
        if !owner_is_compatible(&owner, context) {
            return Ok(Some(owner));
        }
    }
    Ok(None)
}

const fn owner_is_compatible(
    owner: &SessionLeaseOwner,
    context: &SessionLeaseOwnerContext,
) -> bool {
    matches!(
        (owner.storage_writer_epoch, context.storage_writer_epoch),
        (Some(owner_epoch), Some(context_epoch)) if owner_epoch == context_epoch
    )
}

fn remove_file_best_effort(path: &Path) -> Result<(), SessionLeaseError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SessionLeaseError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn format_owner(owner: &SessionLeaseOwner) -> String {
    let mut parts = vec![format!("pid {}", owner.pid)];
    if let Some(epoch) = owner.storage_writer_epoch {
        parts.push(format!("storage writer epoch {epoch}"));
    }
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
fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill(pid, 0)` does not send a signal; it only asks the kernel whether the process
    // exists or is inaccessible. The pid is converted to the platform `i32` representation above.
    let result = unsafe { kill(pid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(1)
}

#[cfg(not(unix))]
const fn process_is_alive(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
fn lock_file_shared(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    const LOCK_SH: i32 = 1;

    // SAFETY: `file.as_raw_fd()` is a valid descriptor for the lifetime of this call, and flock
    // does not retain the pointer or require additional invariants.
    let result = unsafe { flock(file.as_raw_fd(), LOCK_SH) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_file_shared(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        SessionLeaseError::Unsupported.to_string(),
    ))
}

#[cfg(unix)]
fn lock_file_exclusive(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    const LOCK_EX: i32 = 2;

    // SAFETY: `file.as_raw_fd()` is a valid descriptor for the lifetime of this call, and flock
    // does not retain the pointer or require additional invariants.
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

    // SAFETY: `file.as_raw_fd()` is a valid descriptor for the lifetime of this call, and flock
    // does not retain the pointer or require additional invariants.
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
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(build: &str, storage_writer_epoch: u32) -> SessionLeaseOwnerContext {
        SessionLeaseOwnerContext {
            storage_writer_epoch: Some(storage_writer_epoch),
            build_fingerprint: Some(build.to_string()),
            protocol_version: Some(2),
            ..SessionLeaseOwnerContext::default()
        }
    }

    #[test]
    fn allows_multiple_builds_with_same_storage_writer_epoch() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let first = acquire_session_lease(temp_dir.path(), session_id, &context("first", 7))
            .expect("first guard");
        let second = acquire_session_lease(temp_dir.path(), session_id, &context("second", 7))
            .expect("compatible writer guard");

        assert_eq!(first.owner().storage_writer_epoch, Some(7));
        assert_eq!(second.owner().storage_writer_epoch, Some(7));
        assert_ne!(
            first.owner().build_fingerprint,
            second.owner().build_fingerprint
        );
    }

    #[test]
    fn rejects_live_different_storage_writer_epoch() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let _first = acquire_session_lease(temp_dir.path(), session_id, &context("old", 1))
            .expect("first guard");
        let error = acquire_session_lease(temp_dir.path(), session_id, &context("new", 2))
            .expect_err("different storage writer epoch should be rejected");

        assert!(matches!(
            error,
            SessionLeaseError::OwnedByOtherDaemon { .. }
        ));
    }

    #[test]
    fn rejects_missing_storage_writer_identity() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let error = acquire_session_lease(
            temp_dir.path(),
            session_id,
            &SessionLeaseOwnerContext {
                storage_writer_epoch: None,
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect_err("anonymous storage writer must be rejected");
        assert!(matches!(
            error,
            SessionLeaseError::MissingStorageWriterEpoch { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn prunes_dead_owner_before_compatibility_check() {
        use std::process::Command;

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn short-lived process");
        let dead_pid = child.id();
        assert!(child.wait().expect("wait for child").success());
        assert!(
            !process_is_alive(dead_pid),
            "child pid must be dead for test"
        );

        let access_dir = session_owner_dir(temp_dir.path(), session_id);
        let owner = SessionLeaseOwner {
            lease_token: format!("dead-owner-{dead_pid}"),
            pid: dead_pid,
            ..SessionLeaseOwner::new(session_id, &context("dead-incompatible", 1))
        };
        let owner_path = access_dir.join(format!("{}.json", owner.lease_token));
        write_owner_metadata(&owner_path, &owner).expect("write dead owner record");

        let live = acquire_session_lease(temp_dir.path(), session_id, &context("live-current", 2))
            .expect("dead incompatible owner must be pruned");
        assert!(!owner_path.exists(), "dead owner record must be removed");
        assert_eq!(live.owner().storage_writer_epoch, Some(2));
    }

    #[test]
    fn maintenance_refuses_any_live_session_owner() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let _owner = acquire_session_lease(temp_dir.path(), session_id, &context("owner", 7))
            .expect("session owner");
        assert!(matches!(
            acquire_session_maintenance_guard(temp_dir.path(), session_id)
                .expect_err("maintenance must refuse live owner"),
            SessionLeaseError::OwnedByOtherDaemon { .. }
        ));
    }

    #[test]
    fn maintenance_to_lease_transition_prevents_incompatible_handoff_race() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let maintenance = acquire_session_maintenance_guard(temp_dir.path(), session_id)
            .expect("maintenance guard");
        let lease = transition_session_maintenance_to_lease(
            maintenance,
            temp_dir.path(),
            session_id,
            &context("current", 7),
        )
        .expect("transition to runtime lease");
        assert_eq!(lease.owner().storage_writer_epoch, Some(7));
        assert!(matches!(
            acquire_session_lease(temp_dir.path(), session_id, &context("incompatible", 8))
                .expect_err("incompatible writer must not claim transitioned session"),
            SessionLeaseError::OwnedByOtherDaemon { .. }
        ));
        drop(lease);
        acquire_session_lease(temp_dir.path(), session_id, &context("next", 8))
            .expect("new epoch may claim after runtime lease release");
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_lock_helper() {
        let Ok(action) = std::env::var("BCODE_SESSION_LOCK_TEST_ACTION") else {
            return;
        };
        let root = PathBuf::from(std::env::var_os("BCODE_SESSION_LOCK_TEST_ROOT").expect("root"));
        let session_id = std::env::var("BCODE_SESSION_LOCK_TEST_ID")
            .expect("session id")
            .parse::<SessionId>()
            .expect("valid session id");
        let ready =
            PathBuf::from(std::env::var_os("BCODE_SESSION_LOCK_TEST_READY").expect("ready"));
        let acquired =
            PathBuf::from(std::env::var_os("BCODE_SESSION_LOCK_TEST_ACQUIRED").expect("acquired"));
        fs::write(&ready, b"ready").expect("write ready marker");
        match action.as_str() {
            "lease" => {
                let _guard = acquire_session_lease(&root, session_id, &context("subprocess", 7))
                    .expect("acquire subprocess lease");
                fs::write(acquired, b"acquired").expect("write acquired marker");
            }
            "write" => {
                let _guard = acquire_session_write_lock(&root, session_id)
                    .expect("acquire subprocess write lock");
                fs::write(acquired, b"acquired").expect("write acquired marker");
            }
            other => panic!("unknown subprocess lock action {other}"),
        }
    }

    #[cfg(unix)]
    fn assert_subprocess_blocked_by_maintenance(action: &str) {
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let maintenance = acquire_session_maintenance_guard(temp_dir.path(), session_id)
            .expect("maintenance guard");
        let ready = temp_dir.path().join(format!("{action}-ready"));
        let acquired = temp_dir.path().join(format!("{action}-acquired"));
        let mut child = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "lease::tests::subprocess_lock_helper",
                "--nocapture",
            ])
            .env("BCODE_SESSION_LOCK_TEST_ACTION", action)
            .env("BCODE_SESSION_LOCK_TEST_ROOT", temp_dir.path())
            .env("BCODE_SESSION_LOCK_TEST_ID", session_id.to_string())
            .env("BCODE_SESSION_LOCK_TEST_READY", &ready)
            .env("BCODE_SESSION_LOCK_TEST_ACQUIRED", &acquired)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lock subprocess");
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                Instant::now() < ready_deadline,
                "subprocess did not become ready"
            );
            assert!(
                child.try_wait().expect("inspect subprocess").is_none(),
                "subprocess exited before attempting the lock"
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(150));
        assert!(!acquired.exists(), "subprocess bypassed maintenance lock");
        assert!(
            child
                .try_wait()
                .expect("inspect blocked subprocess")
                .is_none(),
            "subprocess exited while maintenance still held the lock"
        );
        drop(maintenance);
        let exit_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().expect("wait for subprocess") {
                assert!(status.success(), "subprocess lock helper failed: {status}");
                break;
            }
            assert!(
                Instant::now() < exit_deadline,
                "subprocess remained blocked after release"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            acquired.exists(),
            "subprocess did not acquire released lock"
        );
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_blocks_session_lease_in_another_process() {
        assert_subprocess_blocked_by_maintenance("lease");
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_blocks_session_write_lock_in_another_process() {
        assert_subprocess_blocked_by_maintenance("write");
    }

    #[test]
    fn maintenance_coordinator_blocks_new_session_lease_until_release() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let maintenance = acquire_session_maintenance_guard(temp_dir.path(), session_id)
            .expect("maintenance guard");
        let root = temp_dir.path().to_path_buf();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            started_tx.send(()).expect("started signal");
            let result = acquire_session_lease(&root, session_id, &context("writer", 7));
            finished_tx.send(result.is_ok()).expect("finished signal");
        });
        started_rx.recv().expect("lease attempt started");
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "lease acquisition must remain blocked while maintenance owns the coordinator"
        );
        drop(maintenance);
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("lease attempt should finish after maintenance release")
        );
        join.join().expect("lease thread");
    }

    #[test]
    fn allows_different_storage_writer_epoch_after_guard_drop() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        {
            let _first = acquire_session_lease(temp_dir.path(), session_id, &context("old", 1))
                .expect("first guard");
        }

        acquire_session_lease(temp_dir.path(), session_id, &context("new", 2))
            .expect("different writer after drop");
    }
}
