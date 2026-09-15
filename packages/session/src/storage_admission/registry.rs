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

/// Maintenance admission borrowing a live daemon's health registration for its entire lifetime.
/// Foreign live or abandoned daemon records still block admission; this is not registry-wide trust.
pub struct AcknowledgedStorageMaintenance<'a> {
    _admission: StorageMaintenanceAdmission,
    _registration: &'a mut crate::storage_daemon_registration::StorageDaemonRegistration,
}

/// Transferable maintenance admission retaining both registry exclusion and daemon liveness.
pub struct OwnedStorageMaintenance {
    _admission: StorageMaintenanceAdmission,
    acknowledgements: Vec<crate::storage_daemon_registration::StorageDaemonAcknowledgement>,
}
impl OwnedStorageMaintenance {
    /// Recheck live tracking health before a committed side effect.
    ///
    /// # Errors
    /// Refuses tracking failures observed since admission.
    pub fn check(&self) -> io::Result<()> {
        self.acknowledgements
            .iter()
            .try_for_each(crate::storage_daemon_registration::StorageDaemonAcknowledgement::check)
    }
}

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

    /// Remove only verified clean, completed daemon records under exclusive registry admission.
    ///
    /// The complete directory scan must fit the budget before any record is removed. Live,
    /// abandoned, malformed and unknown records remain untouched. This never repairs failed
    /// tracking evidence or treats an incomplete scan as proof of safety.
    ///
    /// # Errors
    /// Returns contention, incomplete enumeration, unsafe paths or filesystem errors. Cleanup can
    /// be partially completed on an IO failure; every removed record was independently clean.
    pub fn retire_completed_daemons(&self, entry_budget: usize) -> io::Result<usize> {
        if entry_budget == 0 || entry_budget > 65_536 {
            return Err(invalid());
        }
        let gate = self.gate()?;
        gate.try_lock().map_err(io::Error::from)?;
        let entries = names(&self.directory, entry_budget)?;
        let mut retired = 0;
        for name in entries {
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".daemon")) else {
                continue;
            };
            if id.parse::<SessionId>().is_err() {
                continue;
            }
            let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| invalid())?;
            let file = open_child(&self.directory, &name, libc::O_RDONLY)?;
            if !matches!(
                crate::storage_daemon_registration::daemon_registration_is_complete(file),
                Ok(true)
            ) {
                continue;
            }
            // SAFETY: name is a typed single component, and exclusive registration admission
            // prevents a cooperating daemon from replacing or reopening this completed identity.
            if unsafe { libc::unlinkat(raw(&self.directory), name.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.directory.sync_all()?;
            retired += 1;
        }
        gate.unlock()?;
        Ok(retired)
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
        self.admit_checked(entry_budget, None)
    }

    /// Admit maintenance with an exact healthy live-daemon acknowledgement.
    ///
    /// The guard borrows registration mutably so it cannot be failed, finished or dropped until
    /// maintenance ends. Acknowledgement is tied to the held file's device/inode, not a supplied ID.
    ///
    /// # Errors
    /// Rejects missing/substituted registration, failed health, other active/abandoned daemons,
    /// dirty readers, unknown files, incomplete scans, contention and IO errors.
    pub fn admit_acknowledged<'a>(
        &self,
        entry_budget: usize,
        registration: &'a mut crate::storage_daemon_registration::StorageDaemonRegistration,
    ) -> io::Result<AcknowledgedStorageMaintenance<'a>> {
        let admission = self.admit_checked(entry_budget, Some(&*registration))?;
        Ok(AcknowledgedStorageMaintenance {
            _admission: admission,
            _registration: registration,
        })
    }

    /// Acquire transferable maintenance admission using an exact live registration token.
    ///
    /// # Errors
    /// Rejects stale health, missing identity, foreign live daemons, dirty readers, or incomplete scans.
    pub fn admit_owned(
        &self,
        entry_budget: usize,
        acknowledgement: crate::storage_daemon_registration::StorageDaemonAcknowledgement,
    ) -> io::Result<OwnedStorageMaintenance> {
        self.admit_owned_set(entry_budget, vec![acknowledgement])
    }

    /// Admit a complete set of live-daemon acknowledgements, retaining every liveness handle.
    ///
    /// Each token must match exactly one registry record. Every other daemon must be cleanly
    /// completed; missing participants are never inferred healthy from a process or file name.
    ///
    /// # Errors
    /// Rejects duplicate, foreign, missing or failed acknowledgements, unacknowledged live daemons,
    /// dirty readers, incomplete enumeration, or IO failure.
    pub fn admit_owned_set(
        &self,
        entry_budget: usize,
        acknowledgements: Vec<crate::storage_daemon_registration::StorageDaemonAcknowledgement>,
    ) -> io::Result<OwnedStorageMaintenance> {
        if acknowledgements.len() > entry_budget {
            return Err(invalid());
        }
        let admission = self.admit_owned_checked(entry_budget, &acknowledgements)?;
        Ok(OwnedStorageMaintenance {
            _admission: admission,
            acknowledgements,
        })
    }

    fn admit_checked(
        &self,
        entry_budget: usize,
        registration: Option<&crate::storage_daemon_registration::StorageDaemonRegistration>,
    ) -> io::Result<StorageMaintenanceAdmission> {
        let acknowledgement = registration
            .map(crate::storage_daemon_registration::StorageDaemonRegistration::acknowledgement)
            .transpose()?;
        self.admit_owned_checked(entry_budget, acknowledgement.as_slice())
    }

    fn admit_owned_checked(
        &self,
        entry_budget: usize,
        registrations: &[crate::storage_daemon_registration::StorageDaemonAcknowledgement],
    ) -> io::Result<StorageMaintenanceAdmission> {
        if entry_budget == 0 || entry_budget > 65_536 {
            return Err(invalid());
        }
        let gate = self.gate()?;
        gate.try_lock().map_err(io::Error::from)?;
        let names = names(&self.directory, entry_budget)?;
        let mut files = Vec::new();
        let mut acknowledged = std::collections::BTreeSet::new();
        for name in names {
            if name == "gate" {
                continue;
            }
            let text = name.to_str().ok_or_else(invalid)?;
            if let Some(id) = text.strip_suffix(".daemon") {
                id.parse::<SessionId>().map_err(|_| invalid())?;
                let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| invalid())?;
                let file = open_child(&self.directory, &name, libc::O_RDONLY)?;
                let mut matched = false;
                for (index, registration) in registrations.iter().enumerate() {
                    if registration.acknowledges(&file)? {
                        if matched || !acknowledged.insert(index) {
                            return Err(invalid());
                        }
                        matched = true;
                    }
                }
                if matched {
                    continue;
                }
                if !crate::storage_daemon_registration::daemon_registration_is_complete(file)? {
                    return Err(invalid());
                }
                continue;
            }
            let id = text.strip_suffix(".participant").ok_or_else(invalid)?;
            id.parse::<SessionId>().map_err(|_| invalid())?;
            let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| invalid())?;
            files.push(Ok(open_child(&self.directory, &name, libc::O_RDONLY)?));
        }
        if acknowledged.len() != registrations.len() {
            return Err(invalid());
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
    fn owned_acknowledgement_keeps_liveness_and_observes_health_failure() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let mut daemon = registry.register_daemon(SessionId::new()).expect("daemon");
        let admission = registry
            .admit_owned(16, daemon.acknowledgement().expect("token"))
            .expect("owned admission");
        admission.check().expect("healthy");
        daemon.fail();
        assert!(admission.check().is_err());
        drop(daemon);
        assert!(registry.admit_read(SessionId::new()).is_err());
        drop(admission);
        assert!(registry.admit_maintenance(16).is_err());
    }

    #[test]
    fn live_acknowledgement_is_exact_and_never_covers_foreign_or_failed_daemons() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let mut local = registry.register_daemon(SessionId::new()).expect("local");
        assert!(registry.admit_maintenance(16).is_err());
        {
            let admission = registry
                .admit_acknowledged(16, &mut local)
                .expect("live healthy acknowledgement");
            assert!(registry.admit_read(SessionId::new()).is_err());
            drop(admission);
        }
        let foreign = registry.register_daemon(SessionId::new()).expect("foreign");
        assert!(registry.admit_acknowledged(16, &mut local).is_err());
        foreign.finish().expect("foreign completed");
        drop(
            registry
                .admit_acknowledged(16, &mut local)
                .expect("foreign clean"),
        );
        local.fail();
        assert!(registry.admit_acknowledged(16, &mut local).is_err());
    }

    #[test]
    fn live_acknowledgement_from_another_registry_is_rejected() {
        let root = tempfile::tempdir().expect("root");
        let other = tempfile::tempdir().expect("other");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let other_registry = StorageAdmissionRegistry::open(other.path()).expect("other registry");
        let mut other_daemon = other_registry
            .register_daemon(SessionId::new())
            .expect("other daemon");
        assert!(registry.admit_acknowledged(16, &mut other_daemon).is_err());
    }

    #[test]
    fn completed_daemon_cleanup_preserves_active_abandoned_and_unknown_evidence() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        for _ in 0..3 {
            registry
                .register_daemon(SessionId::new())
                .expect("register")
                .finish()
                .expect("clean");
        }
        let active_id = SessionId::new();
        let active = registry.register_daemon(active_id).expect("active");
        let abandoned_id = SessionId::new();
        drop(registry.register_daemon(abandoned_id).expect("abandoned"));
        let unknown = root.path().join("storage-admission-v1/future-format");
        std::fs::write(&unknown, b"preserve").expect("unknown");
        assert!(registry.retire_completed_daemons(2).is_err());
        assert_eq!(registry.retire_completed_daemons(16).expect("cleanup"), 3);
        assert_eq!(registry.retire_completed_daemons(16).expect("repeat"), 0);
        assert!(
            root.path()
                .join("storage-admission-v1")
                .join(format!("{active_id}.daemon"))
                .exists()
        );
        assert!(
            root.path()
                .join("storage-admission-v1")
                .join(format!("{abandoned_id}.daemon"))
                .exists()
        );
        assert_eq!(std::fs::read(unknown).expect("preserved"), b"preserve");
        drop(active);
        assert!(registry.admit_maintenance(16).is_err());
    }

    #[test]
    fn complete_live_set_admits_maintenance_and_any_failure_invalidates_it() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let first = registry.register_daemon(SessionId::new()).expect("first");
        let mut second = registry.register_daemon(SessionId::new()).expect("second");
        let a = first.acknowledgement().expect("a");
        let b = second.acknowledgement().expect("b");
        assert!(registry.admit_owned_set(16, vec![a.clone()]).is_err());
        assert!(
            registry
                .admit_owned_set(16, vec![a.clone(), a.clone(), b.clone()])
                .is_err()
        );
        let admission = registry
            .admit_owned_set(16, vec![a, b])
            .expect("complete live set");
        admission.check().expect("healthy");
        second.fail();
        assert!(admission.check().is_err());
        drop(admission);
        drop(first);
        drop(second);
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
