//! Daemon-lifetime fail-closed fallback registration.
//!
//! A live daemon holds exclusive liveness ownership. Its ACTIVE record is never alone proof of
//! healthy tracking: coordinated maintenance needs a typed live acknowledgement. Tracking failure
//! leaves ACTIVE durable before any unregistered read is exposed; only a fully drained healthy
//! shutdown may mark the record complete. Process death cannot turn ACTIVE into clean evidence.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, Write as _};

const ACTIVE: &[u8] = b"BCSTDAEMON1:LIVE!";
const CLEAN: &[u8] = b"BCSTDAEMON1:DONE!";

/// A registered daemon. This guard must be installed before accepting content reads.
#[derive(Debug)]
pub struct StorageDaemonRegistration {
    file: File,
    failed: bool,
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
            file,
            failed: false,
        })
    }

    /// Check whether a registry handle is this exact live, healthy registration.
    /// The mutable borrow held by maintenance admission prevents health changes during its scope.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn acknowledges(&self, candidate: &File) -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt as _;
        if self.failed {
            return Ok(false);
        }
        let held = self.file.metadata()?;
        let observed = candidate.metadata()?;
        Ok(held.is_file()
            && observed.is_file()
            && held.nlink() == 1
            && held.dev() == observed.dev()
            && held.ino() == observed.ino())
    }

    /// Mark this daemon unsafe for maintenance. No extra disk write is required: ACTIVE is durable.
    pub const fn fail(&mut self) {
        self.failed = true;
    }

    /// Whether this daemon can acknowledge a coordinated health request.
    #[must_use]
    pub const fn healthy(&self) -> bool {
        !self.failed
    }

    /// Complete a healthy epoch only after all reads, access updates, and maintenance have drained.
    ///
    /// # Errors
    /// Returns an error for failed tracking or IO. Failed/crashed epochs retain ACTIVE permanently.
    pub fn finish(mut self) -> io::Result<()> {
        if self.failed {
            return Err(invalid());
        }
        self.file.rewind()?;
        self.file.write_all(CLEAN)?;
        self.file.sync_all()?;
        self.file.unlock()
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
