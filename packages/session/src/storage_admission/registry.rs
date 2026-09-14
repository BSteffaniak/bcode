//! Filesystem-backed admission registry for one authorized storage root.
//!
//! Registration and enumeration occur under the same coordinator gate. Names are generated from
//! typed session-domain UUID identities, never tool-supplied paths. A bounded incomplete scan cannot
//! authorize maintenance. This registry alone does not prove that older clients participate.

use super::{StorageMaintenanceAdmission, StorageReadAdmission, invalid};
use bcode_session_models::SessionId;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

/// Confined admission registry, opened from an already authorized state location.
pub struct StorageAdmissionRegistry {
    directory: File,
}

impl StorageAdmissionRegistry {
    /// Open or initialize the registry beneath an existing authorized root.
    ///
    /// # Errors
    /// Rejects symlinks, non-directories, unavailable roots, or IO failure. Does not create the root.
    pub fn open(root: &Path) -> io::Result<Self> {
        let root = File::open(root)?;
        if !root.metadata()?.is_dir() {
            return Err(invalid());
        }
        let name = c"storage-admission-v1";
        // SAFETY: a fixed name beneath an owned directory descriptor.
        let created = unsafe { libc::mkdirat(raw(&root), name.as_ptr(), 0o700) };
        if created != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
            return Err(io::Error::last_os_error());
        }
        let directory = open_child(&root, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        root.sync_all()?;
        Ok(Self { directory })
    }

    /// Register daemon-lifetime liveness before serving content reads.
    ///
    /// Registration is serialized with maintenance admission. Active or abandoned daemon records
    /// remain an explicit blocker until coordinated live acknowledgement or clean completion.
    ///
    /// # Errors
    /// Returns lock contention, duplicate identity, unsafe paths or durability failures.
    pub fn register_daemon(
        &self,
        identity: SessionId,
    ) -> io::Result<crate::storage_daemon_registration::StorageDaemonRegistration> {
        let gate = self.gate()?;
        gate.try_lock_shared().map_err(io::Error::from)?;
        let name = std::ffi::CString::new(format!("{identity}.daemon")).map_err(|_| invalid())?;
        let file = open_child(
            &self.directory,
            &name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        )?;
        let registration =
            crate::storage_daemon_registration::StorageDaemonRegistration::begin(file)?;
        self.directory.sync_all()?;
        gate.unlock()?;
        Ok(registration)
    }

    /// Admit one read using a caller-owned stable participant identity.
    ///
    /// Reuse a participant only after its prior operation finishes. Independent participants may
    /// read concurrently. The returned guard must cover both content reads and access persistence.
    ///
    /// # Errors
    /// Returns contention, unsafe files, persistence failures, or unsupported existing state.
    pub fn admit_read(&self, participant: SessionId) -> io::Result<StorageReadAdmission> {
        let gate = self.gate()?;
        gate.try_lock_shared().map_err(io::Error::from)?;
        let name =
            std::ffi::CString::new(format!("{participant}.participant")).map_err(|_| invalid())?;
        let file = open_child(&self.directory, &name, libc::O_RDWR | libc::O_CREAT)?;
        self.directory.sync_all()?;
        // begin reacquires the same shared lock on this handle; it never releases admission.
        StorageReadAdmission::begin(gate, file)
    }

    /// Acquire maintenance only after a complete registry scan fits the explicit entry budget.
    ///
    /// # Errors
    /// Returns an error for active readers, dirty/unknown participants, unknown files, a zero or
    /// exhausted budget, or IO. All registration stays excluded through the returned guard.
    pub fn admit_maintenance(
        &self,
        entry_budget: usize,
    ) -> io::Result<StorageMaintenanceAdmission> {
        if entry_budget == 0 || entry_budget > 65_536 {
            return Err(invalid());
        }
        let gate = self.gate()?;
        gate.try_lock().map_err(io::Error::from)?;
        let names = names(&self.directory, entry_budget)?;
        let mut files = Vec::new();
        for name in names {
            if name == "gate" {
                continue;
            }
            let text = name.to_str().ok_or_else(invalid)?;
            if let Some(id) = text.strip_suffix(".daemon") {
                id.parse::<SessionId>().map_err(|_| invalid())?;
                let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| invalid())?;
                if !crate::storage_daemon_registration::daemon_registration_is_complete(
                    open_child(&self.directory, &name, libc::O_RDONLY)?,
                )? {
                    return Err(invalid());
                }
                continue;
            }
            let id = text.strip_suffix(".participant").ok_or_else(invalid)?;
            id.parse::<SessionId>().map_err(|_| invalid())?;
            let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| invalid())?;
            files.push(Ok(open_child(&self.directory, &name, libc::O_RDONLY)?));
        }
        StorageMaintenanceAdmission::begin(gate, files)
    }

    /// Finish a uniquely owned operation and retire its participant while still excluding maintenance.
    ///
    /// Unlike separate completion/retirement, this does not require upgrading a shared gate while
    /// unrelated readers are active. The participant identity must never be reused concurrently.
    ///
    /// # Errors
    /// Rejects preexisting dirty state, identity substitution, or IO failure. Unknown participants
    /// are not removed. Both the gate and participant remain held until directory sync completes.
    pub fn complete_read(
        &self,
        participant: SessionId,
        mut admission: StorageReadAdmission,
    ) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt as _;
        if !admission.may_clean {
            return Err(invalid());
        }
        let name =
            std::ffi::CString::new(format!("{participant}.participant")).map_err(|_| invalid())?;
        let current = open_child(&self.directory, &name, libc::O_RDONLY)?;
        let current = current.metadata()?;
        let held = admission.participant.metadata()?;
        if current.dev() != held.dev() || current.ino() != held.ino() {
            return Err(invalid());
        }
        // Leave dirty until retirement is durable: interruption cannot expose stale clean evidence.
        // SAFETY: name is a typed single component and the held shared gate excludes maintenance.
        if unsafe { libc::unlinkat(raw(&self.directory), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.directory.sync_all()?;
        admission.may_clean = false;
        admission.participant.unlock()?;
        admission.gate.unlock()
    }

    /// Persist an idempotent state-location blocker for an unregistered read fallback.
    ///
    /// The shared gate refuses an active maintenance operation before the caller exposes content.
    /// Subsequent exclusive scans reject this unknown participant name, including after restart.
    /// The blocker is never automatically removed by successful reads or participant retirement.
    ///
    /// # Errors
    /// Returns contention or IO errors. On failure the caller has no durable fallback proof.
    pub fn block_maintenance_for_fallback(&self) -> io::Result<()> {
        let gate = self.gate()?;
        gate.try_lock_shared().map_err(io::Error::from)?;
        let marker = open_child(
            &self.directory,
            c"unregistered-read.blocked",
            libc::O_RDWR | libc::O_CREAT,
        )?;
        marker.sync_all()?;
        self.directory.sync_all()?;
        gate.unlock()
    }

    /// Register a degraded reader without treating damaged participant state as clean.
    ///
    /// This marker is durable before content is exposed and is never automatically repaired.
    /// It is useful when a previously admitted read cannot commit its access timestamp.
    ///
    /// # Errors
    /// Returns contention or IO failure; no health claim is made on failure.
    pub fn mark_degraded(&self, participant: SessionId) -> io::Result<()> {
        drop(self.admit_read(participant)?);
        Ok(())
    }

    /// Check registry health without acquiring mutation authority.
    ///
    /// # Errors
    /// Returns errors for active readers, dirty state, incomplete enumeration or IO.
    pub fn check_health(&self, entry_budget: usize) -> io::Result<()> {
        drop(self.admit_maintenance(entry_budget)?);
        Ok(())
    }

    /// Retire a completed participant without leaving registry growth proportional to read count.
    ///
    /// Retirement holds exclusive admission so enumeration cannot race removal. Dirty or active
    /// participants are never removed. Callers must stop reusing this identity before retirement.
    ///
    /// # Errors
    /// Returns contention, dirty/unknown state, missing participants or IO failures. Failed
    /// retirement preserves the participant and does not grant maintenance admission.
    pub fn retire(&self, participant: SessionId) -> io::Result<()> {
        let gate = self.gate()?;
        gate.try_lock().map_err(io::Error::from)?;
        let name =
            std::ffi::CString::new(format!("{participant}.participant")).map_err(|_| invalid())?;
        let mut file = open_child(&self.directory, &name, libc::O_RDONLY)?;
        file.try_lock_shared().map_err(io::Error::from)?;
        super::read_state(&mut file, false)?;
        // SAFETY: the fixed typed identity is one component in the pinned registry directory.
        // Exclusive admission excludes registered readers and other retirement operations.
        if unsafe { libc::unlinkat(raw(&self.directory), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.directory.sync_all()?;
        file.unlock()?;
        gate.unlock()
    }

    fn gate(&self) -> io::Result<File> {
        let file = open_child(&self.directory, c"gate", libc::O_RDWR | libc::O_CREAT)?;
        self.directory.sync_all()?;
        Ok(file)
    }
}

fn raw(file: &File) -> std::os::fd::RawFd {
    use std::os::fd::AsRawFd as _;
    file.as_raw_fd()
}

fn open_child(parent: &File, name: &std::ffi::CStr, flags: i32) -> io::Result<File> {
    use std::os::fd::FromRawFd as _;
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: parent is owned and name is one NUL-terminated component.
    let descriptor = unsafe {
        libc::openat(
            raw(parent),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: descriptor is fresh and ownership transfers exactly once.
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    if flags & libc::O_DIRECTORY != 0 {
        if !metadata.is_dir() {
            return Err(invalid());
        }
    } else if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(invalid());
    }
    Ok(file)
}

struct Directory(*mut libc::DIR);
impl Drop for Directory {
    fn drop(&mut self) {
        // SAFETY: this guard uniquely owns the live DIR.
        unsafe {
            libc::closedir(self.0);
        }
    }
}

fn names(directory: &File, budget: usize) -> io::Result<Vec<std::ffi::OsString>> {
    use std::os::fd::{FromRawFd as _, IntoRawFd as _};
    use std::os::unix::ffi::OsStringExt as _;
    let fd = open_child(directory, c".", libc::O_RDONLY | libc::O_DIRECTORY)?.into_raw_fd();
    // SAFETY: fd is a directory descriptor transferred to fdopendir on success.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: on failure fdopendir leaves fd owned by the caller.
        drop(unsafe { File::from_raw_fd(fd) });
        return Err(error);
    }
    let stream = Directory(stream);
    let mut names = Vec::new();
    loop {
        // SAFETY: clear this thread's errno to distinguish EOF from failed enumeration.
        unsafe {
            *errno() = 0;
        }
        // SAFETY: stream is valid and exclusively owned.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(error);
            }
            break;
        }
        // SAFETY: d_name is initialized and NUL-terminated until the next readdir.
        let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if names.len() == budget {
            return Err(invalid());
        }
        names.push(std::ffi::OsString::from_vec(bytes.to_vec()));
    }
    Ok(names)
}

#[cfg(target_os = "macos")]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: returns the calling thread's errno location.
    unsafe { libc::__error() }
}
#[cfg(target_os = "linux")]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: returns the calling thread's errno location.
    unsafe { libc::__errno_location() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retiring_clean_participants_bounds_registry_growth_without_clearing_damage() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        for _ in 0..20 {
            let id = SessionId::new();
            registry
                .admit_read(id)
                .expect("read")
                .complete()
                .expect("complete");
            registry.retire(id).expect("retire");
        }
        drop(
            registry
                .admit_maintenance(1)
                .expect("only coordinator remains"),
        );
        let dirty = SessionId::new();
        let active = registry.admit_read(dirty).expect("active");
        assert!(registry.retire(dirty).is_err());
        drop(active);
        assert!(registry.retire(dirty).is_err());
        assert!(registry.admit_maintenance(16).is_err());
    }

    #[test]
    fn completed_reads_retire_while_other_readers_remain_active() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let long_id = SessionId::new();
        let long = registry.admit_read(long_id).expect("long read");
        for _ in 0..100 {
            let id = SessionId::new();
            let short = registry.admit_read(id).expect("short read");
            registry
                .complete_read(id, short)
                .expect("retire without exclusive upgrade");
        }
        assert_eq!(
            std::fs::read_dir(root.path().join("storage-admission-v1"))
                .expect("registry")
                .count(),
            2
        );
        registry
            .complete_read(long_id, long)
            .expect("long complete");
        drop(
            registry
                .admit_maintenance(1)
                .expect("no leaked participants"),
        );
    }

    #[test]
    fn completion_rejects_wrong_participant_identity() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let a = SessionId::new();
        let b = SessionId::new();
        let first = registry.admit_read(a).expect("a");
        let second = registry.admit_read(b).expect("b");
        assert!(registry.complete_read(b, first).is_err());
        drop(second);
        assert!(registry.admit_maintenance(8).is_err());
        assert_eq!(
            std::fs::read_dir(root.path().join("storage-admission-v1"))
                .expect("registry")
                .count(),
            3
        );
    }

    #[test]
    fn durable_fallback_blocker_survives_restart_and_is_idempotent() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        registry.block_maintenance_for_fallback().expect("block");
        registry.block_maintenance_for_fallback().expect("repeat");
        assert!(registry.admit_maintenance(4096).is_err());
        let id = SessionId::new();
        let read = registry.admit_read(id).expect("read remains available");
        registry.complete_read(id, read).expect("complete");
        drop(registry);
        let registry = StorageAdmissionRegistry::open(root.path()).expect("restart");
        assert!(registry.admit_maintenance(4096).is_err());
        assert!(
            root.path()
                .join("storage-admission-v1/unregistered-read.blocked")
                .is_file()
        );
    }

    #[test]
    fn fallback_cannot_authorize_read_while_maintenance_is_active() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let maintenance = registry.admit_maintenance(4096).expect("maintenance");
        assert!(registry.block_maintenance_for_fallback().is_err());
        drop(maintenance);
        registry
            .block_maintenance_for_fallback()
            .expect("after maintenance");
        assert!(registry.admit_maintenance(4096).is_err());
    }

    #[test]
    fn daemon_registration_blocks_scan_until_clean_completion_and_survives_failure() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let daemon = registry.register_daemon(SessionId::new()).expect("daemon");
        assert!(registry.admit_maintenance(16).is_err());
        let read_id = SessionId::new();
        let read = registry.admit_read(read_id).expect("reads coexist");
        registry
            .complete_read(read_id, read)
            .expect("complete read");
        daemon.finish().expect("healthy drained daemon");
        drop(registry.admit_maintenance(16).expect("clean scan"));
        let mut failed = registry
            .register_daemon(SessionId::new())
            .expect("second daemon");
        failed.fail();
        assert!(failed.finish().is_err());
        drop(registry);
        let registry = StorageAdmissionRegistry::open(root.path()).expect("restart");
        assert!(registry.admit_maintenance(16).is_err());
    }

    #[test]
    fn registry_tracks_multiple_readers_and_dirty_restart() {
        let root = tempfile::tempdir().expect("root");
        let first = StorageAdmissionRegistry::open(root.path()).expect("first");
        let second = StorageAdmissionRegistry::open(root.path()).expect("second");
        let a = first.admit_read(SessionId::new()).expect("a");
        let b = second.admit_read(SessionId::new()).expect("b");
        assert!(first.admit_maintenance(16).is_err());
        a.complete().expect("a done");
        b.complete().expect("b done");
        drop(first.admit_maintenance(16).expect("clean"));
        drop(second.admit_read(SessionId::new()).expect("abandoned"));
        drop(first);
        drop(second);
        let reopened = StorageAdmissionRegistry::open(root.path()).expect("reopen");
        assert!(reopened.admit_maintenance(16).is_err());
        reopened
            .admit_read(SessionId::new())
            .expect("unrelated read")
            .complete()
            .expect("complete");
    }

    #[test]
    fn incomplete_or_unknown_registry_never_grants_maintenance() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        for _ in 0..3 {
            registry
                .admit_read(SessionId::new())
                .expect("read")
                .complete()
                .expect("done");
        }
        assert!(registry.admit_maintenance(2).is_err());
        drop(registry.admit_maintenance(4).expect("complete scan"));
        std::fs::write(root.path().join("storage-admission-v1/future"), b"unknown")
            .expect("unknown");
        assert!(registry.admit_maintenance(16).is_err());
    }

    #[test]
    fn registry_rejects_symlink_and_hardlink_participants() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        let id = SessionId::new();
        let path = root
            .path()
            .join("storage-admission-v1")
            .join(format!("{id}.participant"));
        symlink(outside.path(), &path).expect("symlink");
        assert!(registry.admit_read(id).is_err());
        std::fs::remove_file(&path).expect("fixture cleanup");
        std::fs::hard_link(outside.path(), &path).expect("hardlink");
        assert!(registry.admit_read(id).is_err());
        assert_eq!(outside.as_file().metadata().expect("metadata").len(), 0);
    }
}
