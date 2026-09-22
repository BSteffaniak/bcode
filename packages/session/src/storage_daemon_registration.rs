//! Daemon-lifetime fail-closed fallback registration.
//!
//! A live daemon holds exclusive liveness ownership. Its ACTIVE record is never alone proof of
//! healthy tracking: coordinated maintenance needs a typed live acknowledgement. Tracking failure
//! leaves ACTIVE durable before any unregistered read is exposed; only a fully drained healthy
//! shutdown may mark the record complete. Process death cannot turn ACTIVE into clean evidence.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, Write as _};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const ACTIVE: &[u8] = b"BCSTDAEMON1:LIVE!";
const CLEAN: &[u8] = b"BCSTDAEMON1:DONE!";

/// A registered daemon. This guard must be installed before accepting content reads.
#[derive(Debug)]
pub struct StorageDaemonRegistration {
    file: Option<Arc<File>>,
    failed: Arc<AtomicBool>,
}

impl Drop for StorageDaemonRegistration {
    fn drop(&mut self) {
        self.failed.store(true, Ordering::SeqCst);
    }
}

impl StorageDaemonRegistration {
    /// Initialize an exclusively created participant file and retain lifetime liveness ownership.
    ///
    /// # Errors
    /// Rejects nonempty/nonregular files, lock contention, and persistence failures.
    pub fn begin(mut file: File) -> io::Result<Self> {
        if !file.metadata()?.is_file() || file.metadata()?.len() != 0 {
            return Err(invalid());
        }
        file.try_lock().map_err(io::Error::from)?;
        file.write_all(ACTIVE)?;
        file.sync_all()?;
        Ok(Self {
            file: Some(Arc::new(file)),
            failed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Mark this daemon unsafe for maintenance. No extra disk write is required: ACTIVE is durable.
    pub fn fail(&mut self) {
        self.failed.store(true, Ordering::SeqCst);
    }

    /// Whether this daemon can acknowledge a coordinated health request.
    #[must_use]
    pub fn healthy(&self) -> bool {
        !self.failed.load(Ordering::SeqCst)
    }

    /// Obtain an owned liveness token for async maintenance; health is checked again by consumers.
    ///
    /// # Errors
    /// Refuses a daemon that has observed tracking failure.
    pub fn acknowledgement(&self) -> io::Result<StorageDaemonAcknowledgement> {
        if !self.healthy() {
            return Err(invalid());
        }
        Ok(StorageDaemonAcknowledgement {
            file: Arc::clone(self.file.as_ref().ok_or_else(invalid)?),
            failed: Arc::clone(&self.failed),
        })
    }

    /// Complete a healthy epoch only after all reads, access updates, and maintenance have drained.
    ///
    /// # Errors
    /// Returns an error for failed tracking, altered durable registration, outstanding tokens or IO.
    /// Failed/crashed/corrupt epochs are preserved rather than rewritten as CLEAN.
    pub fn finish(mut self) -> io::Result<()> {
        if !self.healthy() {
            return Err(invalid());
        }
        let mut file =
            Arc::try_unwrap(self.file.take().ok_or_else(invalid)?).map_err(|_| invalid())?;
        validate_active_record(&mut file)?;
        file.rewind()?;
        file.write_all(CLEAN)?;
        file.sync_all()?;
        file.unlock()
    }
}

fn validate_active_record(file: &mut File) -> io::Result<()> {
    if file.metadata()?.len() != ACTIVE.len() as u64 {
        return Err(invalid());
    }
    file.rewind()?;
    let mut bytes = [0; ACTIVE.len()];
    file.read_exact(&mut bytes)?;
    if bytes != ACTIVE {
        return Err(invalid());
    }
    Ok(())
}

/// Owned live registration proof. Retaining it keeps the OS liveness lock held.
/// Health failure invalidates every outstanding token; callers must recheck before publication.
#[derive(Clone, Debug)]
pub struct StorageDaemonAcknowledgement {
    // Retained on every platform to keep the registration's liveness lock held.
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux")),
        allow(
            dead_code,
            reason = "ownership retains the registration lock until the last acknowledgement drops"
        )
    )]
    file: Arc<File>,
    failed: Arc<AtomicBool>,
}

impl StorageDaemonAcknowledgement {
    /// Validate that tracking has not failed since the token was obtained.
    ///
    /// # Errors
    /// Returns an error after any tracking failure for this daemon.
    pub fn check(&self) -> io::Result<()> {
        if self.failed.load(Ordering::SeqCst) {
            Err(invalid())
        } else {
            Ok(())
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn acknowledges(&self, candidate: &File) -> io::Result<bool> {
        use std::os::unix::fs::{FileExt as _, MetadataExt as _};
        self.check()?;
        let held = self.file.metadata()?;
        let observed = candidate.metadata()?;
        if held.nlink() != 1
            || !observed.is_file()
            || held.dev() != observed.dev()
            || held.ino() != observed.ino()
        {
            return Ok(false);
        }
        // Positional IO does not disturb the shared liveness descriptor's cursor. The owned lock
        // excludes cooperating writers; hostile mutation still must never be accepted as ACTIVE.
        if observed.len() != ACTIVE.len() as u64 {
            return Err(invalid());
        }
        let mut bytes = [0; ACTIVE.len()];
        candidate.read_exact_at(&mut bytes, 0)?;
        if bytes != ACTIVE {
            return Err(invalid());
        }
        self.check()?;
        Ok(true)
    }
}

/// Inspect a daemon record without inferring live health from mere process existence.
///
/// Returns `true` only for a cleanly completed record. A live ACTIVE record needs a separate typed
/// acknowledgement from its daemon while coordination excludes new fallback reads; this function
/// deliberately does not pretend that a lock alone conveys that acknowledgement.
///
/// # Errors
/// Returns contention, damaged/unsupported state, or IO failure.
pub fn daemon_registration_is_complete(mut file: File) -> io::Result<bool> {
    file.try_lock_shared().map_err(io::Error::from)?;
    if file.metadata()?.len() != CLEAN.len() as u64 {
        return Err(invalid());
    }
    file.rewind()?;
    let mut bytes = [0; CLEAN.len()];
    file.read_exact(&mut bytes)?;
    file.unlock()?;
    if bytes == CLEAN {
        Ok(true)
    } else if bytes == ACTIVE {
        Ok(false)
    } else {
        Err(invalid())
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "daemon storage tracking is not verifiably healthy",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_registration_invalidates_tokens_but_retains_lock_until_they_drain() {
        let file = tempfile::NamedTempFile::new().expect("file");
        let daemon =
            StorageDaemonRegistration::begin(file.reopen().expect("open")).expect("register");
        let token = daemon.acknowledgement().expect("token");
        token.check().expect("live");
        drop(daemon);
        assert!(token.check().is_err());
        let probe = file.reopen().expect("probe");
        assert!(probe.try_lock().is_err());
        drop(token);
        probe.try_lock().expect("released after drain");
        probe.unlock().expect("unlock");
        assert!(!daemon_registration_is_complete(probe).expect("dirty evidence"));
    }

    #[test]
    fn refused_finish_invalidates_outstanding_acknowledgement() {
        let file = tempfile::NamedTempFile::new().expect("file");
        let daemon =
            StorageDaemonRegistration::begin(file.reopen().expect("open")).expect("register");
        let token = daemon.acknowledgement().expect("token");
        assert!(daemon.finish().is_err());
        assert!(token.check().is_err());
        drop(token);
        assert!(!daemon_registration_is_complete(file.reopen().expect("inspect")).expect("dirty"));
    }

    #[test]
    fn clean_completion_never_overwrites_corrupt_or_future_registration() {
        for bytes in [
            b"".as_slice(),
            b"truncated",
            b"BCSTDAEMON2:LIVE!",
            b"BCSTDAEMON1:DONE!",
        ] {
            let file = tempfile::NamedTempFile::new().expect("file");
            let daemon =
                StorageDaemonRegistration::begin(file.reopen().expect("open")).expect("begin");
            let mut owned_file = daemon.file.as_deref().expect("owned file");
            std::io::Seek::seek(&mut owned_file, std::io::SeekFrom::Start(0))
                .expect("rewind fixture");
            owned_file.set_len(0).expect("truncate fixture");
            owned_file.write_all(bytes).expect("damage fixture");
            assert!(daemon.finish().is_err());
            assert_eq!(std::fs::read(file.path()).expect("preserved"), bytes);
            let probe = file.reopen().expect("probe");
            probe.try_lock().expect("failed finish relinquishes lock");
            probe.unlock().expect("unlock");
        }
    }

    #[test]
    fn only_clean_shutdown_can_clear_registration() {
        let file = tempfile::NamedTempFile::new().expect("file");
        let daemon = StorageDaemonRegistration::begin(file.reopen().expect("open")).expect("begin");
        assert!(daemon.healthy());
        assert!(daemon_registration_is_complete(file.reopen().expect("inspect")).is_err());
        daemon.finish().expect("finish");
        assert!(
            daemon_registration_is_complete(file.reopen().expect("inspect")).expect("complete")
        );
    }

    #[test]
    fn failed_or_abandoned_registration_stays_unsafe_without_another_write() {
        for fail in [false, true] {
            let file = tempfile::NamedTempFile::new().expect("file");
            let mut daemon =
                StorageDaemonRegistration::begin(file.reopen().expect("open")).expect("begin");
            if fail {
                daemon.fail();
                assert!(!daemon.healthy());
                assert!(daemon.finish().is_err());
            } else {
                drop(daemon);
            }
            assert!(
                !daemon_registration_is_complete(file.reopen().expect("inspect"))
                    .expect("active record")
            );
            assert_eq!(std::fs::read(file.path()).expect("persisted"), ACTIVE);
        }
    }
}
